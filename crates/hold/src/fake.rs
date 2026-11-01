//! An in-memory [`Runtime`] and [`Machine`], for testing Plumb without an
//! engine. It records every mutating call, so a test can assert that a
//! converged pass does nothing at all, and it can be told to fail.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::{
    Container, ContainerSpec, Error, Inspection, Labels, Machine, MachineState, Result, Runtime, State, label,
};

#[derive(Debug, Clone)]
pub struct FakeContainer {
    pub id: String,
    pub spec: ContainerSpec,
    pub state: State,
    pub labels: Labels,
    pub restarts: i64,
    pub aliases: Vec<String>,
    /// Log lines with the Unix second they were written.
    pub logs: Vec<(i64, String)>,
    pub oom_killed: bool,
    pub exit_code: Option<i64>,
}

#[derive(Default)]
struct Inner {
    containers: BTreeMap<String, FakeContainer>,
    networks: BTreeSet<String>,
    volumes: BTreeSet<String>,
    images: BTreeSet<String>,
    next_id: u64,
    ops: Vec<String>,
    // Faults.
    list_failures: u32,
    unpullable: BTreeSet<String>,
    unstartable: BTreeSet<String>,
    down: bool,
}

#[derive(Default)]
pub struct Fake {
    inner: Mutex<Inner>,
}

impl Fake {
    pub fn new() -> Fake {
        Fake::default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut self.inner.lock().unwrap())
    }

    /// Mutating calls since the last `take_ops`, e.g. `create draw-api-r1`.
    pub fn take_ops(&self) -> Vec<String> {
        self.with(|i| std::mem::take(&mut i.ops))
    }

    pub fn containers(&self) -> Vec<FakeContainer> {
        self.with(|i| i.containers.values().cloned().collect())
    }

    pub fn container(&self, name: &str) -> Option<FakeContainer> {
        self.with(|i| i.containers.values().find(|c| c.spec.name == name).cloned())
    }

    pub fn networks(&self) -> Vec<String> {
        self.with(|i| i.networks.iter().cloned().collect())
    }

    pub fn volumes(&self) -> Vec<String> {
        self.with(|i| i.volumes.iter().cloned().collect())
    }

    /// The next `n` calls to `list` fail.
    pub fn fail_list(&self, n: u32) {
        self.with(|i| i.list_failures = n);
    }

    pub fn unpullable(&self, image: &str) {
        self.with(|i| i.unpullable.insert(image.to_owned()));
    }

    /// Containers with this name fail to start until `fix_start`.
    pub fn unstartable(&self, name: &str) {
        self.with(|i| i.unstartable.insert(name.to_owned()));
    }

    pub fn fix_start(&self, name: &str) {
        self.with(|i| i.unstartable.remove(name));
    }

    /// The container dies, as if it crashed and the engine gave up on it.
    pub fn crash(&self, name: &str) {
        self.with(|i| {
            if let Some(c) = i.containers.values_mut().find(|c| c.spec.name == name) {
                c.state = State::Exited;
                c.restarts += 1;
            }
        });
    }

    /// The process exits on its own, without an engine restart.
    pub fn exit(&self, name: &str, code: i64) {
        self.with(|i| {
            if let Some(c) = i.containers.values_mut().find(|c| c.spec.name == name) {
                c.state = State::Exited;
                c.exit_code = Some(code);
            }
        });
    }

    /// The container is frozen, as Ark does for the moment of a backup.
    pub fn pause(&self, name: &str, paused: bool) {
        self.with(|i| {
            if let Some(c) = i.containers.values_mut().find(|c| c.spec.name == name) {
                c.state = if paused { State::Paused } else { State::Running };
            }
        });
    }

    /// The engine restarts the container after it crashes: it keeps running,
    /// but its restart count goes up.
    pub fn bounce(&self, name: &str) {
        self.with(|i| {
            if let Some(c) = i.containers.values_mut().find(|c| c.spec.name == name) {
                c.restarts += 1;
            }
        });
    }

    /// The container writes a log line now.
    pub fn log(&self, name: &str, line: &str) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        self.with(|i| {
            if let Some(c) = i.containers.values_mut().find(|c| c.spec.name == name) {
                c.logs.push((now, line.to_owned()));
            }
        });
    }

    /// The kernel killed the container for using too much memory.
    pub fn oom(&self, name: &str) {
        self.with(|i| {
            if let Some(c) = i.containers.values_mut().find(|c| c.spec.name == name) {
                c.state = State::Exited;
                c.oom_killed = true;
            }
        });
    }

    /// The engine (and its VM) goes away.
    pub fn set_down(&self, down: bool) {
        self.with(|i| i.down = down);
    }

    /// A container someone else started, unmanaged or with Henry's labels.
    pub fn plant(&self, name: &str, labels: Labels) {
        self.with(|i| {
            i.next_id += 1;
            let id = format!("planted{}", i.next_id);
            let spec = ContainerSpec {
                name: name.to_owned(),
                image: "planted".into(),
                command: None,
                env: vec![],
                secret_stamp: String::new(),
                labels: labels.clone(),
                network: String::new(),
                aliases: vec![],
                mounts: vec![],
                publish: None,
                memory_bytes: 0,
                cpus: 0.0,
            };
            i.containers.insert(
                id.clone(),
                FakeContainer {
                    id,
                    spec,
                    state: State::Running,
                    labels,
                    restarts: 0,
                    aliases: vec![],
                    logs: vec![],
                    oom_killed: false,
                    exit_code: None,
                },
            );
        });
    }

    fn up(&self) -> Result<()> {
        if self.with(|i| i.down) { Err(Error::Unreachable("fake engine is down".into())) } else { Ok(()) }
    }
}

fn find<'a>(i: &'a mut Inner, id: &str) -> Result<&'a mut FakeContainer> {
    i.containers.get_mut(id).ok_or_else(|| Error::NotFound(id.to_owned()))
}

impl Runtime for Fake {
    async fn ping(&self) -> Result<()> {
        self.up()
    }

    async fn list(&self) -> Result<Vec<Container>> {
        self.up()?;
        self.with(|i| {
            if i.list_failures > 0 {
                i.list_failures -= 1;
                return Err(Error::Unreachable("list failed".into()));
            }
            Ok(i.containers
                .values()
                .filter(|c| c.labels.get(label::MANAGED).is_some_and(|v| v == "1"))
                .map(|c| Container {
                    id: c.id.clone(),
                    name: c.spec.name.clone(),
                    state: c.state,
                    labels: c.labels.clone(),
                    networks: [(c.spec.network.clone(), c.aliases.clone())].into_iter().collect(),
                })
                .collect())
        })
    }

    async fn ensure_network(&self, name: &str, _labels: &Labels) -> Result<()> {
        self.up()?;
        self.with(|i| {
            if i.networks.insert(name.to_owned()) {
                i.ops.push(format!("network {name}"));
            }
        });
        Ok(())
    }

    async fn ensure_volume(&self, name: &str, _labels: &Labels) -> Result<()> {
        self.up()?;
        self.with(|i| {
            if i.volumes.insert(name.to_owned()) {
                i.ops.push(format!("volume {name}"));
            }
        });
        Ok(())
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        self.up()?;
        self.with(|i| {
            if i.unpullable.contains(image) {
                return Err(Error::Engine(format!("pull {image}: manifest unknown")));
            }
            if i.images.insert(image.to_owned()) {
                i.ops.push(format!("pull {image}"));
            }
            Ok(())
        })
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String> {
        self.up()?;
        self.with(|i| {
            if i.containers.values().any(|c| c.spec.name == spec.name) {
                return Err(Error::Conflict(spec.name.clone()));
            }
            if !i.images.contains(&spec.image) {
                return Err(Error::NotFound(format!("image {}", spec.image)));
            }
            i.next_id += 1;
            let id = format!("c{}", i.next_id);
            let mut labels = spec.labels.clone();
            labels.insert(label::MANAGED.into(), "1".into());
            labels.insert(label::SPEC.into(), spec.hash());
            i.containers.insert(
                id.clone(),
                FakeContainer {
                    id: id.clone(),
                    spec: spec.clone(),
                    state: State::Created,
                    labels,
                    restarts: 0,
                    aliases: spec.aliases.clone(),
                    logs: vec![],
                    oom_killed: false,
                    exit_code: None,
                },
            );
            i.ops.push(format!("create {}", spec.name));
            Ok(id)
        })
    }

    async fn start(&self, id: &str) -> Result<()> {
        self.up()?;
        self.with(|i| {
            let c = find(i, id)?;
            let name = c.spec.name.clone();
            if c.state == State::Running {
                return Ok(());
            }
            if i.unstartable.contains(&name) {
                return Err(Error::Engine(format!("{name}: exec format error")));
            }
            find(i, id)?.state = State::Running;
            i.ops.push(format!("start {name}"));
            Ok(())
        })
    }

    async fn stop(&self, id: &str, _grace: Duration) -> Result<()> {
        self.up()?;
        self.with(|i| {
            let c = find(i, id)?;
            if c.state == State::Running {
                c.state = State::Exited;
                let name = c.spec.name.clone();
                i.ops.push(format!("stop {name}"));
            }
            Ok(())
        })
    }

    async fn remove(&self, id: &str) -> Result<()> {
        self.up()?;
        self.with(|i| {
            if let Some(c) = i.containers.remove(id) {
                i.ops.push(format!("remove {}", c.spec.name));
            }
        });
        Ok(())
    }

    async fn set_aliases(&self, id: &str, _network: &str, aliases: &[String]) -> Result<()> {
        self.up()?;
        self.with(|i| {
            let c = find(i, id)?;
            c.aliases = aliases.to_vec();
            let name = c.spec.name.clone();
            i.ops.push(format!("aliases {name} {}", aliases.join(",")));
            Ok(())
        })
    }

    async fn inspect(&self, id: &str) -> Result<Inspection> {
        self.up()?;
        self.with(|i| {
            let c = find(i, id)?;
            Ok(Inspection {
                state: c.state,
                restart_count: c.restarts,
                exit_code: (c.state == State::Exited).then_some(if c.oom_killed {
                    137
                } else {
                    c.exit_code.unwrap_or(1)
                }),
                oom_killed: c.oom_killed,
            })
        })
    }

    async fn logs(&self, id: &str, tail: usize, since: Option<i64>) -> Result<String> {
        self.up()?;
        self.with(|i| {
            let c = find(i, id)?;
            let lines: Vec<&str> =
                c.logs.iter().filter(|(t, _)| since.is_none_or(|s| *t >= s)).map(|(_, l)| l.as_str()).collect();
            Ok(lines[lines.len().saturating_sub(tail)..].iter().map(|l| format!("{l}\n")).collect())
        })
    }
}

impl Machine for Fake {
    async fn ensure_up(&self) -> Result<MachineState> {
        self.up().map(|()| MachineState::Up)
    }

    async fn socket(&self) -> Result<PathBuf> {
        Ok("/fake.sock".into())
    }
}
