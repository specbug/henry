# Provider and build recovery on an existing Henry host

These commands are for the hosting Mac, after the reviewed Henry changes
have been transferred there. The cloud checkout cannot access its state,
Keychain, daemon, containers or logs. Nothing here requires showing or
re-entering a saved provider key.

## What the repository establishes

- Meta's current [quickstart](https://dev.meta.ai/docs/quickstart),
  [Chat Completions protocol](https://dev.meta.ai/docs/protocols/chat-completions),
  and [tool guide](https://dev.meta.ai/docs/tool-calling), checked on
  2026-10-31, specify `https://api.meta.ai/v1` and `muse-spark-1.3`.
  `/chat/completions` supports the function-tool loop Henry uses: assistant
  tool calls, JSON argument strings, and one `tool` result per call ID,
  including parallel calls. `tool_choice` defaults to `auto`, which Henry
  leaves unset. No provider-wire migration or standalone key check is needed.
  The previous Llama defaults were stale. A 401 at that old endpoint alone
  says nothing conclusive about the saved key at the current endpoint.
- `henry reload` previously left the job worker and polling task with their
  startup configuration. The worker now takes a validated configuration
  snapshot for each job, without interrupting an in-flight build; polling
  also follows reloads. This bug was reproduced with queued jobs selecting
  the old provider after an update. It is not proof that a reload caused
  either historical Emend failure.
- Earlier setup omitted Railpack. Current main already installs a managed
  copy and accepts the official `checksums.txt` asset (rather than only
  `railpack_<version>_checksums.txt`). The additional regression here verifies
  installation from that asset and rejection of corrupted downloads. See the
  [official release](https://github.com/railwayapp/railpack/releases/tag/v0.43.0).
  Current main also updates BuildKit and replaces containers using an older
  configured image. These upstream fixes are retained.
- Sound's manifest-only plan delegates startup detection to Railpack and
  assumes port 8080 via `PORT`. It does not spend AI tokens. Press calls
  `railpack build <directory> --name <image> --progress plain` using BuildKit.
  Hold omits `Cmd` when the lease has no `command`, and does not override
  `Entrypoint`, preserving the image's startup metadata. No demonstrated
  command-dropping bug was found in that path.
- Proof already rejects a web process that stops with exit 0. Regression
  fixtures now cover this even with an answering health port, delivery of
  the failure from a Sound/Railpack trial to the repair planner, and resuming
  a provider-blocked repair without forgetting its history or budget.

Emend's source and release-16 image/logs were not available in the cloud.
Commit `89898c4f36` identifies the reported Emend release, not Henry's version.
Exit 0 establishes that the process finished successfully; it does not tell
us why it finished, what command the image ran, or which command is correct.
No speculative Emend startup command or relaxed proof gate was added.

## Rebuild and configure on the Mac

From the Mac's Henry repository containing these changes:

```sh
cargo install --path crates/henry --locked --force
henry_bin="$HOME/.cargo/bin/henry"
"$henry_bin" --version
"$henry_bin" where
"$henry_bin" tools adopt railpack
```

`cargo install` is the installation used by `scripts/setup.sh`. If you use a
custom Cargo installation root, set `henry_bin` to that binary instead. Version
0.1.0 alone does not identify this revision; also check `"$henry_bin" retry --help`.

Edit the config path printed by `where`. Merge into the existing tables;
do not replace other settings or append duplicate TOML table headers:

```toml
[ai]
default_model = "meta/muse-spark-1.3"

[ai.endpoints]
meta = "https://api.meta.ai/v1"
```

These explicit values are useful when the installation had an old model or
endpoint pinned. An installation intentionally using a different provider,
model or proxy should keep its overrides. Unset values use the new defaults.
The existing `henry/META_API_KEY` secret name remains unchanged.

No `[press] railpack` setting is needed for the managed installation. If an
existing setting points to a missing or unwanted binary, remove just that
setting to use the managed copy, or set it to the correct absolute path:

```toml
[press]
railpack = "/absolute/path/to/railpack"
```

Then load the new executable and configuration:

```sh
"$henry_bin" install
"$henry_bin" reload
"$henry_bin" show emend
"$henry_bin" plans emend
```

`install` uses the current executable for the LaunchAgent and restarts the
configured Henry/Caddy/cloudflared agents. Schedule this brief restart as
appropriate for the host. `reload` alone cannot load a newly compiled binary.
Later config-only edits need only `reload`; the next job sees them.

## Diagnose before retrying Emend

First collect the historical plan, build output and remaining image metadata.
The following reads only the selected release and safe image fields, not
secret tables, environment values, or container configuration dumps:

```sh
henry_data=$("$henry_bin" where | sed -n 's/^home *//p')
"$henry_bin" show emend
"$henry_bin" plans emend
sqlite3 -readonly "$henry_data/canon.db" \
  "SELECT id, commit_sha, state, images, reason FROM releases WHERE app='emend' AND id=16;"
test ! -f "$henry_data/logs/builds/emend-r16-web.log" || \
  tail -n 120 "$henry_data/logs/builds/emend-r16-web.log"
```

Use the exact `web` image reference from the `images` JSON, if it still exists:

```sh
podman image inspect '<web image reference>' \
  --format 'Entrypoint={{json .Config.Entrypoint}} Cmd={{json .Config.Cmd}} WorkingDir={{json .Config.WorkingDir}}'
```

Do not recreate the removed `emend-web-r16` merely to inspect it. If its logs
were not captured before removal, they may be gone. If the image is also
gone, the minimal remaining evidence is Emend's manifests, its start/build
scripts and Railpack configuration at the full release commit. Do not share
`.env` files, secrets or full inspect output. Review build-log excerpts for
application-printed secrets before sharing them.

To generate a fresh Railpack plan, use a separate development checkout of
the actual Emend repository at the release commit, not Henry's daemon-owned
checkout (which Press cleans on deploy). With the managed installation:

```sh
"$henry_data/tools/railpack/current/railpack" --version
"$henry_data/tools/railpack/current/railpack" plan /absolute/path/to/emend-at-release-commit
```

Use your explicit Railpack binary instead if configured. A plan generated
with today's Railpack is new diagnostic evidence, not a reconstruction of
the historical image; record the version. Inspect `deploy.startCommand`
against Emend's scripts before changing any startup command.

After fixing prerequisites, choose the action matching `henry show emend`:

```sh
# Stuck onboarding: resume planning/repair with its recorded failures.
# This can spend AI tokens. It preserves round/token caps and prior plans.
"$henry_bin" retry emend

# Alternatively, to retry the existing lease without AI replanning:
# "$henry_bin" deploy emend
```

Do not run both. `retry` refuses an onboarding that is not idle and stuck;
it does not delete the app or reset exhausted budgets. If a repair cap has
been reached, review the recorded attempts and deliberately adjust the
existing `[ai] max_rounds` or `max_tokens` only if more repair is intended,
then reload. If `show` reports a trial already in progress, observe it rather
than launching a duplicate.

Follow the actual new release rather than assuming it is release 17:

```sh
"$henry_bin" show emend
"$henry_bin" plans emend
"$henry_bin" events emend
"$henry_bin" ps
```

Success requires a running web container, healthy probes, soak and restart
proof, and `live` status. If the new container stops, capture its logs before
it is removed; do not send secret values. Public verification on the Mac is
separate: after Henry reports Live, visit `https://emend.sixeleven.in` using
the expected Access/login flow and verify actual app behavior. A redirect
or a successful image build alone does not prove the app works. No cloud
test here deploys or verifies that hostname.
