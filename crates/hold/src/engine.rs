//! The engine behind a Docker-compatible socket: Podman on the Mac, Docker
//! in CI. Only the API calls Henry needs, mapped onto Hold's small vocabulary.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use bollard::Docker;
use bollard::errors::Error as BollardError;
use bollard::models::{
    ContainerCreateBody, ContainerStateStatusEnum, ContainerSummaryStateEnum, EndpointSettings, HostConfig,
    Mount as EngineMount, MountType, NetworkConnectRequest, NetworkCreateRequest, NetworkDisconnectRequest,
    NetworkingConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder, StopContainerOptionsBuilder,
};
use futures_util::TryStreamExt;

use crate::{Container, ContainerSpec, Error, Inspection, Labels, Result, Runtime, State, label};

/// Seconds an ordinary API call may take before Henry gives up on it.
const API_TIMEOUT: u64 = 120;
/// Pulls can be slow on a cold cache; they get longer.
const PULL_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Clone)]
pub struct Engine {
    docker: Docker,
}

impl Engine {
    pub fn connect(socket: &Path) -> Result<Engine> {
        let path = socket.to_str().ok_or_else(|| Error::Unreachable(format!("{} is not UTF-8", socket.display())))?;
        let docker = Docker::connect_with_unix(path, API_TIMEOUT, bollard::API_DEFAULT_VERSION)
            .map_err(|e| Error::Unreachable(format!("{}: {e}", socket.display())))?;
        Ok(Engine { docker })
    }
}

fn map(what: &str, e: BollardError) -> Error {
    match e {
        BollardError::DockerResponseServerError { status_code: 404, .. } => Error::NotFound(what.to_owned()),
        BollardError::DockerResponseServerError { status_code: 409, .. } => Error::Conflict(what.to_owned()),
        BollardError::DockerResponseServerError { status_code, message } => {
            Error::Engine(format!("{what}: {status_code} {message}"))
        }
        BollardError::RequestTimeoutError => Error::Timeout(what.to_owned()),
        other => Error::Unreachable(format!("{what}: {other}")),
    }
}

fn labels_map(labels: &Labels) -> HashMap<String, String> {
    labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

impl Runtime for Engine {
    async fn ping(&self) -> Result<()> {
        self.docker.ping().await.map(|_| ()).map_err(|e| map("ping", e))
    }

    async fn list(&self) -> Result<Vec<Container>> {
        let filters = HashMap::from([("label", vec![format!("{}=1", label::MANAGED)])]);
        let opts = ListContainersOptionsBuilder::new().all(true).filters(&filters).build();
        let found = self.docker.list_containers(Some(opts)).await.map_err(|e| map("list containers", e))?;
        let mut out = Vec::with_capacity(found.len());
        for c in found {
            let (Some(id), Some(name)) = (c.id, c.names.and_then(|n| n.into_iter().next())) else { continue };
            let state = match c.state {
                Some(ContainerSummaryStateEnum::CREATED) => State::Created,
                Some(ContainerSummaryStateEnum::RUNNING) => State::Running,
                Some(ContainerSummaryStateEnum::PAUSED) => State::Paused,
                Some(ContainerSummaryStateEnum::RESTARTING) => State::Restarting,
                Some(ContainerSummaryStateEnum::EXITED) => State::Exited,
                _ => State::Other,
            };
            let listed = c.network_settings.and_then(|n| n.networks).unwrap_or_default();
            // Docker's listing leaves aliases out; only inspect has them.
            let networks = if listed.values().any(|ep| ep.aliases.is_none()) {
                let detail = self.docker.inspect_container(&id, None).await.map_err(|e| map(&name, e))?;
                detail
                    .network_settings
                    .and_then(|n| n.networks)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(net, ep)| (net, ep.aliases.unwrap_or_default()))
                    .collect()
            } else {
                listed.into_iter().map(|(net, ep)| (net, ep.aliases.unwrap_or_default())).collect()
            };
            out.push(Container {
                id,
                name: name.trim_start_matches('/').to_owned(),
                state,
                labels: c.labels.unwrap_or_default().into_iter().collect(),
                networks,
            });
        }
        Ok(out)
    }

    async fn ensure_network(&self, name: &str, labels: &Labels) -> Result<()> {
        match self.docker.inspect_network(name, None).await {
            Ok(_) => return Ok(()),
            Err(BollardError::DockerResponseServerError { status_code: 404, .. }) => {}
            Err(e) => return Err(map(name, e)),
        }
        let req = NetworkCreateRequest {
            name: name.to_owned(),
            driver: Some("bridge".into()),
            labels: Some(labels_map(labels)),
            ..Default::default()
        };
        match self.docker.create_network(req).await {
            // Lost a race with another creator: the network exists, which is all we wanted.
            Ok(_) | Err(BollardError::DockerResponseServerError { status_code: 409, .. }) => Ok(()),
            Err(e) => Err(map(name, e)),
        }
    }

    async fn ensure_volume(&self, name: &str, labels: &Labels) -> Result<()> {
        match self.docker.inspect_volume(name).await {
            Ok(_) => return Ok(()),
            Err(BollardError::DockerResponseServerError { status_code: 404, .. }) => {}
            Err(e) => return Err(map(name, e)),
        }
        let req =
            VolumeCreateRequest { name: Some(name.to_owned()), labels: Some(labels_map(labels)), ..Default::default() };
        self.docker.create_volume(req).await.map(|_| ()).map_err(|e| map(name, e))
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        match self.docker.inspect_image(image).await {
            Ok(_) => return Ok(()),
            Err(BollardError::DockerResponseServerError { status_code: 404, .. }) => {}
            Err(e) => return Err(map(image, e)),
        }
        let opts = CreateImageOptionsBuilder::new().from_image(image).build();
        let pull = self.docker.create_image(Some(opts), None, None).try_collect::<Vec<_>>();
        match tokio::time::timeout(PULL_TIMEOUT, pull).await {
            Err(_) => Err(Error::Timeout(format!("pulling {image}"))),
            Ok(Err(e)) => Err(map(&format!("pulling {image}"), e)),
            Ok(Ok(_)) => Ok(()),
        }
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String> {
        let mut labels = spec.labels.clone();
        labels.insert(label::MANAGED.into(), "1".into());
        labels.insert(label::SPEC.into(), spec.hash());

        let port_bindings = spec.publish.as_ref().map(|p| {
            HashMap::from([(
                format!("{}/tcp", p.container_port),
                Some(vec![PortBinding { host_ip: Some(p.host_ip.clone()), host_port: Some(p.host_port.to_string()) }]),
            )])
        });
        let mounts = spec
            .mounts
            .iter()
            .map(|m| EngineMount {
                source: Some(m.volume.clone()),
                target: Some(m.target.clone()),
                typ: Some(MountType::VOLUME),
                ..Default::default()
            })
            .collect();
        let host_config = HostConfig {
            memory: Some(spec.memory_bytes as i64),
            nano_cpus: Some((spec.cpus * 1e9) as i64),
            network_mode: Some(spec.network.clone()),
            port_bindings,
            mounts: Some(mounts),
            // The engine restarts a crash at once; Plumb backs it up if the
            // engine itself restarts and forgets.
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            init: Some(true),
            ..Default::default()
        };
        let networking_config = NetworkingConfig {
            endpoints_config: Some(HashMap::from([(
                spec.network.clone(),
                EndpointSettings { aliases: Some(spec.aliases.clone()), ..Default::default() },
            )])),
        };
        let body = ContainerCreateBody {
            image: Some(spec.image.clone()),
            cmd: spec.command.clone(),
            env: Some(spec.env.iter().map(|e| format!("{}={}", e.name, e.value)).collect()),
            labels: Some(labels.into_iter().collect()),
            exposed_ports: spec.publish.as_ref().map(|p| vec![format!("{}/tcp", p.container_port)]),
            host_config: Some(host_config),
            networking_config: Some(networking_config),
            ..Default::default()
        };
        let opts = CreateContainerOptionsBuilder::new().name(&spec.name).build();
        let created = self.docker.create_container(Some(opts), body).await.map_err(|e| map(&spec.name, e))?;
        Ok(created.id)
    }

    async fn start(&self, id: &str) -> Result<()> {
        match self.docker.start_container(id, None).await {
            // 304: already running.
            Ok(()) | Err(BollardError::DockerResponseServerError { status_code: 304, .. }) => Ok(()),
            Err(e) => Err(map(id, e)),
        }
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<()> {
        let opts = StopContainerOptionsBuilder::new().t(grace.as_secs() as i32).build();
        match self.docker.stop_container(id, Some(opts)).await {
            Ok(()) | Err(BollardError::DockerResponseServerError { status_code: 304, .. }) => Ok(()),
            Err(e) => Err(map(id, e)),
        }
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let opts = RemoveContainerOptionsBuilder::new().force(true).v(false).build();
        match self.docker.remove_container(id, Some(opts)).await {
            Ok(()) | Err(BollardError::DockerResponseServerError { status_code: 404, .. }) => Ok(()),
            Err(e) => Err(map(id, e)),
        }
    }

    async fn set_aliases(&self, id: &str, network: &str, aliases: &[String]) -> Result<()> {
        let off = NetworkDisconnectRequest { container: id.to_owned(), force: Some(true) };
        match self.docker.disconnect_network(network, off).await {
            Ok(()) | Err(BollardError::DockerResponseServerError { status_code: 404, .. }) => {}
            Err(e) => return Err(map(network, e)),
        }
        let on = NetworkConnectRequest {
            container: id.to_owned(),
            endpoint_config: Some(EndpointSettings { aliases: Some(aliases.to_vec()), ..Default::default() }),
        };
        self.docker.connect_network(network, on).await.map_err(|e| map(network, e))
    }

    async fn inspect(&self, id: &str) -> Result<Inspection> {
        let c = self.docker.inspect_container(id, None).await.map_err(|e| map(id, e))?;
        let s = c.state.unwrap_or_default();
        let state = match s.status {
            Some(ContainerStateStatusEnum::CREATED) => State::Created,
            Some(ContainerStateStatusEnum::RUNNING) => State::Running,
            Some(ContainerStateStatusEnum::PAUSED) => State::Paused,
            Some(ContainerStateStatusEnum::RESTARTING) => State::Restarting,
            Some(ContainerStateStatusEnum::EXITED) => State::Exited,
            _ => State::Other,
        };
        Ok(Inspection {
            state,
            restart_count: c.restart_count.unwrap_or(0),
            exit_code: s.exit_code,
            oom_killed: s.oom_killed.unwrap_or(false),
        })
    }

    async fn logs(&self, id: &str, tail: usize, since: Option<i64>) -> Result<String> {
        let mut opts = LogsOptionsBuilder::new().stdout(true).stderr(true).tail(&tail.to_string());
        if let Some(since) = since {
            opts = opts.since(since as i32);
        }
        let opts = opts.build();
        let chunks = self.docker.logs(id, Some(opts)).try_collect::<Vec<_>>().await.map_err(|e| map(id, e))?;
        Ok(chunks.iter().map(ToString::to_string).collect())
    }
}
