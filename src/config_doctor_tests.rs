use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use tokio::sync::mpsc;

use super::*;

#[derive(Default)]
struct Gate {
    released: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn wait(&self) -> Result<()> {
        let mut released = self
            .released
            .lock()
            .map_err(|_| anyhow::anyhow!("gate poisoned"))?;
        while !*released {
            released = self
                .changed
                .wait(released)
                .map_err(|_| anyhow::anyhow!("gate poisoned"))?;
        }
        Ok(())
    }

    fn release(&self) {
        match self.released.lock() {
            Ok(mut released) => {
                *released = true;
                self.changed.notify_all();
            }
            Err(error) => panic!("gate poisoned: {error}"),
        }
    }
}

struct Gates(BTreeMap<String, Arc<Gate>>);

impl Drop for Gates {
    fn drop(&mut self) {
        for gate in self.0.values() {
            gate.release();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Entered(String),
    Finished(String),
}

struct Completion {
    name: String,
    active: Arc<AtomicUsize>,
    events: mpsc::UnboundedSender<Event>,
}

impl Drop for Completion {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        if let Err(error) = self.events.send(Event::Finished(self.name.clone())) {
            eprintln!("inspection completion receiver closed: {error}");
        }
    }
}

struct Run {
    gates: Gates,
    events: mpsc::UnboundedReceiver<Event>,
    peak: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<Result<Vec<(String, Value)>>>,
}

impl Run {
    fn start(names: &[&str], failures: &[&str], panic_name: Option<&str>) -> Result<Self> {
        let entries: serde_json::Map<_, _> = names
            .iter()
            .map(|name| (name.to_string(), json!({"command":"echo"})))
            .collect();
        let configuration = crate::server_config::parse_document(
            std::path::Path::new("doctor-test.json"),
            &json!({"mcpServers":entries}),
        )?;
        let gates: BTreeMap<_, _> = names
            .iter()
            .map(|name| (name.to_string(), Arc::new(Gate::default())))
            .collect();
        let worker_gates = gates.clone();
        let failures: Vec<_> = failures.iter().map(|name| name.to_string()).collect();
        let panic_name = panic_name.map(str::to_owned);
        let (events, receiver) = mpsc::unbounded_channel();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let worker_peak = peak.clone();
        let task = tokio::spawn(async move {
            inspect_with(&configuration, move |server| {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                worker_peak.fetch_max(count, Ordering::SeqCst);
                let _completion = Completion {
                    name: server.name.clone(),
                    active: active.clone(),
                    events: events.clone(),
                };
                events.send(Event::Entered(server.name.clone()))?;
                worker_gates
                    .get(&server.name)
                    .context("missing gate")?
                    .wait()?;
                if panic_name.as_deref() == Some(&server.name) {
                    panic!("controlled inspection panic");
                }
                if failures.contains(&server.name) {
                    bail!("{} inspection failed", server.name);
                }
                Ok(json!({"inspected":server.name}))
            })
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|(server, status)| (server.name.clone(), status))
                    .collect()
            })
        });
        Ok(Self {
            gates: Gates(gates),
            events: receiver,
            peak,
            task,
        })
    }

    fn release(&self, name: &str) -> Result<()> {
        self.gates
            .0
            .get(name)
            .context("missing release gate")?
            .release();
        Ok(())
    }

    async fn event(&mut self) -> Result<Event> {
        tokio::time::timeout(Duration::from_secs(5), self.events.recv())
            .await?
            .context("inspection events closed")
    }

    async fn entered(&mut self, expected: &[&str]) -> Result<()> {
        let mut names = Vec::new();
        for _ in expected {
            match self.event().await? {
                Event::Entered(name) => names.push(name),
                event => bail!("expected admission, got {event:?}"),
            }
        }
        names.sort();
        ensure!(names == expected, "unexpected admissions: {names:?}");
        Ok(())
    }

    async fn finish(self) -> Result<(Result<Vec<(String, Value)>>, Vec<Event>, usize)> {
        let Self {
            gates,
            mut events,
            peak,
            task,
        } = self;
        drop(gates);
        let result = task.await?;
        let mut remaining = Vec::new();
        while let Ok(event) = events.try_recv() {
            remaining.push(event);
        }
        Ok((result, remaining, peak.load(Ordering::SeqCst)))
    }
}

#[tokio::test]
async fn doctor_inspections_overlap_and_keep_name_order() -> Result<()> {
    let mut run = Run::start(&["beta", "alpha"], &[], None)?;
    let checked = async {
        run.entered(&["alpha", "beta"]).await?;
        run.release("beta")?;
        ensure!(run.event().await? == Event::Finished("beta".to_owned()));
        ensure!(!run.task.is_finished(), "alpha still held");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let (rows, _, peak) = run.finish().await?;
    checked?;
    assert_eq!(
        rows?,
        vec![
            ("alpha".to_owned(), json!({"inspected":"alpha"})),
            ("beta".to_owned(), json!({"inspected":"beta"}))
        ]
    );
    assert_eq!(peak, 2);
    Ok(())
}

#[tokio::test]
async fn doctor_inspections_bound_concurrency_and_preserve_results() -> Result<()> {
    let names = ["alpha", "beta", "delta", "gamma", "zeta"];
    let mut run = Run::start(&names, &[], None)?;
    let checked = run.entered(&["alpha", "beta", "delta", "gamma"]).await;
    let (rows, _, peak) = run.finish().await?;
    checked?;
    assert_eq!(
        rows?
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        names
    );
    assert!(peak <= 4, "inspection concurrency exceeded its bound");
    Ok(())
}

#[tokio::test]
async fn doctor_error_priority_is_input_order_and_started_jobs_drain() -> Result<()> {
    for panic_name in [None, Some("alpha"), Some("beta")] {
        let mut run = Run::start(
            &["alpha", "beta", "delta", "gamma", "zeta"],
            &["alpha", "beta"],
            panic_name,
        )?;
        let checked = async {
            run.entered(&["alpha", "beta", "delta", "gamma"]).await?;
            run.release("beta")?;
            ensure!(run.event().await? == Event::Finished("beta".to_owned()));
            ensure!(!run.task.is_finished(), "must wait for earlier alpha");
            run.release("alpha")?;
            ensure!(run.event().await? == Event::Finished("alpha".to_owned()));
            ensure!(!run.task.is_finished(), "must drain later held inspections");
            ensure!(
                run.events.try_recv().is_err(),
                "next batch admitted after failure"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let (result, mut remaining, peak) = run.finish().await?;
        checked?;
        let error = result.err().context("doctor unexpectedly succeeded")?;
        if panic_name == Some("alpha") {
            assert_eq!(error.to_string(), "Credential inspection task failed");
            assert!(format!("{error:#}").contains("controlled inspection panic"));
        } else {
            assert_eq!(error.to_string(), "alpha inspection failed");
        }
        remaining.sort_by_key(|event| format!("{event:?}"));
        assert_eq!(
            remaining,
            vec![
                Event::Finished("delta".to_owned()),
                Event::Finished("gamma".to_owned())
            ]
        );
        assert_eq!(peak, 4);
    }
    Ok(())
}
