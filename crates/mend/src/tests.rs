use canon::{ReleaseState, Source};

use super::*;

#[test]
fn due_tasks_are_queued_once_and_failures_wait() {
    let c = Canon::open_in_memory().unwrap();
    let all = [Task::Disk, Task::Host, Task::Tools];
    assert_eq!(schedule(&c, &all).unwrap(), all);
    assert_eq!(c.claim().unwrap().unwrap().app.as_deref(), Some("disk"), "a Mend job's app is its task");
    c.record_upkeep("disk", true, "ok").unwrap();
    c.record_upkeep("host", false, "fdesetup missing").unwrap();
    c.record_upkeep("tools", true, "ok").unwrap();
    assert!(schedule(&c, &all).unwrap().is_empty(), "done recently, or failed recently");
    assert_eq!(Task::parse("refresh"), Some(Task::Refresh));
    assert!(Task::Bench.every() > Task::Disk.every());
}

#[test]
fn images_any_release_could_run_are_kept() {
    let c = Canon::open_in_memory().unwrap();
    let lease = "name = \"draw\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = { dockerfile = \"Dockerfile\" }\nhealth = { http = \"/\" }\n";
    c.insert_app(&lease::Lease::parse(lease).unwrap(), lease, "repo", Source::Operator).unwrap();
    let mut ids = Vec::new();
    for n in 1..=5 {
        let r = c.create_release("draw", &format!("sha{n}"), lease).unwrap();
        c.set_release_images(r.id, &serde_json::json!({ "web": format!("henry/draw-web:{n}") })).unwrap();
        ids.push(r.id);
    }
    c.set_release_state(ids[0], ReleaseState::Live, None).unwrap();
    let keep = disk::keep(&c, 2).unwrap();
    let all: Vec<String> = (1..=5)
        .map(|n| format!("henry/draw-web:{n}"))
        .chain(["postgres:17".into(), "henry/ark-helper:1".into(), "henry/gone-web:x".into()])
        .collect();
    let stale = disk::stale(&all, &keep);
    assert_eq!(
        stale,
        ["henry/draw-web:2", "henry/draw-web:3", "henry/gone-web:x"],
        "live, newest two and others' images stay"
    );
}
