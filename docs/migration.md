# Moving odyssey and draw off hostd

One app at a time. hostd keeps serving each app until Henry has proven it,
and the switch can be undone. This runbook is for the Mac mini, in Terminal
(not over a LaunchAgent: Terminal may read `~/Documents`, a background agent
may not).

What carries over:

| hostd | Henry |
| --- | --- |
| `compose.yml` | the lease Sound writes from it: the same services, env, volume and healthcheck. The app's own `cloudflared` is left out; Henry's tunnel replaces it |
| `.env`, by hand | sealed in Hush by `henry adopt`, never shown again. `${VAR:-default}` in compose fills what `.env` leaves out, as compose did |
| the `odyssey-data` / `draw-data` volume | copied into `henry-<app>-<volume>` by `henry adopt`, then again at the cutover |
| the DNS record you made for hostd's tunnel | left alone until `henry cutover`, which points it at Henry's tunnel and keeps the old one for `--back` |
| backupd (rclone mirror to R2) | Ark: restic snapshots to R2 with history, and a restore drill |
| the Cloudflare Access app in front of draw | unchanged: Access works on the hostname, whatever tunnel serves it |

## Before the first app

Henry is installed and running (`henry install`), with:

- `[cloudflare]` and `henry/CLOUDFLARE_API_TOKEN`, so Henry's tunnel exists.
  Your zone must be listed in `zones`.
- `[ark]` and its secrets, so the apps are backed up from their first day on
  Henry. See the README's "Setting up backups".
- For draw, whose web build is long: `[press] build_timeout_minutes = 45`.

hostd's stacks publish ports 8000, 3000, 8100 and 3100 on the Mac; Henry
publishes from its own range, so both run side by side on one Podman machine.
Both start that machine when it is down, and two starts at once can collapse
it. Avoid rebooting mid-migration. If you must and `podman ps` hangs
afterwards, run `podman machine stop && podman machine start` once by hand.

## odyssey

1. **Adopt.** Henry seals the `.env`, copies the volume while hostd keeps
   serving, and onboards the repo.

   ```sh
   henry adopt https://github.com/specbug/odyssey \
     --hostname odyssey.sixeleven.in --branch main \
     --env ~/Documents/odyssey/.env --volume odyssey-data
   ```

2. **Watch it go live, privately.** The public still reaches hostd.

   ```sh
   henry show odyssey        # or Glass: henry open
   henry plans odyssey       # the lease Sound wrote, if you want to read it
   ```

   It is ready when `henry show odyssey` says `live`. If it gets stuck, the
   last event says why. Fix the repo (or add a `lease.toml`), push, and
   `henry deploy odyssey`. hostd is unaffected throughout.

3. **Let hostd go.** hostd's watchdog would restart the stack, so unregister
   it first:

   ```sh
   cd ~/Documents/hostd && hostd rm odyssey       # run inside the hostd repo
   git commit -am "hostd: odyssey moves to Henry" && hostd install
   cd ~/Documents/odyssey && podman compose down
   ```

   odyssey is down for the public from here until the next step finishes,
   about a minute.

4. **Cut over.** Henry copies the final data (it refuses while anything of
   hostd's still runs on the volume), waits for odyssey to be live again,
   then points the DNS record at its own tunnel.

   ```sh
   henry cutover odyssey
   curl -fsS https://odyssey.sixeleven.in/api/health
   ```

5. **Prove the backup.** Within a day Ark backs up the volume and drills the
   snapshot. `henry backup odyssey` does it now.

   ```sh
   henry backups odyssey     # wait for "drill passed"
   ```

**Going back**, if something is wrong after the cutover:

```sh
henry cutover odyssey --back        # the DNS record returns to hostd's tunnel
cd ~/Documents/odyssey && podman compose up -d
cd ~/Documents/hostd && hostd add ~/Documents/odyssey && git commit -am "hostd: odyssey back" && hostd install
```

What odyssey wrote while on Henry stays in Henry's volume
`henry-odyssey-odyssey-data`. A later `henry cutover` copies hostd's data
over it, so take anything you need from it first.

## draw

The same steps, after odyssey has run on Henry for a few days:

```sh
henry adopt https://github.com/specbug/excalidraw --name draw \
  --hostname draw.sixeleven.in --branch master \
  --env ~/Documents/excalidraw/.env --volume draw-data
henry show draw                                   # until live
cd ~/Documents/hostd && hostd rm draw && git commit -am "hostd: draw moves to Henry" && hostd install
cd ~/Documents/excalidraw && podman compose down
henry cutover draw
henry backups draw                                # until a drill passes
```

draw has no login of its own. Before the cutover, check that its Cloudflare
Access application still covers `draw.sixeleven.in`: the cutover changes
which tunnel serves the hostname, not who may reach it.

## Retiring hostd

Once both apps have passed a restore drill on Henry:

```sh
cd ~/Documents/hostd && hostd rm backupd
for a in ~/Library/LaunchAgents/in.sixeleven.hostd-*.plist; do launchctl bootout gui/$(id -u) "$a" && rm "$a"; done
podman compose -f ~/Documents/hostd/compose.yml down
```

Leave backupd's last mirror in R2 as an archive, and hostd's volumes
(`podman volume ls`) until you are sure; `podman volume rm odyssey-data
draw-data` frees the space. hostd's own repo stays in git history.
