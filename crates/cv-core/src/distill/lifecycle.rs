//! Provider-neutral call/job ownership and completion tracking.
use crate::harness::lifecycle::{Observation, Status, Target, Update};
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Default)]
pub(super) struct State {
    calls: BTreeSet<String>,
    jobs: HashMap<String, String>,
    job_counts: HashMap<String, usize>,
    uncertain: HashSet<String>,
    notices: HashSet<String>,
    terminal: HashMap<String, (Status, Option<String>)>,
}

pub(super) struct Completion {
    pub call: String,
    pub status: Status,
    pub output: Option<String>,
}

pub(super) struct Outcome {
    pub status: Status,
    pub pending: bool,
    pub output: Option<String>,
    pub completions: Vec<Completion>,
}

impl State {
    pub fn call(&mut self, id: &str) {
        self.calls.insert(id.to_owned());
    }

    pub fn result(&mut self, id: &str, mut observation: Observation) -> Outcome {
        let owner = observation
            .acting_on
            .as_ref()
            .and_then(|job| self.jobs.get(job))
            .cloned()
            .unwrap_or_else(|| id.to_owned());
        if observation.status.is_terminal() {
            self.calls.remove(id);
        } else {
            self.calls.insert(id.to_owned());
        }
        // A wait can reveal a new child before completing its parent cell. Preserve ownership
        // and attach starts first so parent completion cannot settle the whole invocation early.
        let started = !observation.started_jobs.is_empty();
        for job in observation.started_jobs {
            if let std::collections::hash_map::Entry::Vacant(entry) = self.jobs.entry(job) {
                entry.insert(owner.clone());
                *self.job_counts.entry(owner.clone()).or_default() += 1;
            }
        }
        if observation.uncertain {
            self.uncertain.insert(owner.clone());
        }
        for update in &mut observation.updates {
            if matches!(update.status, Status::Failed | Status::Stopped) && update.output.is_none() {
                update.output = observation.output.clone();
            }
        }
        let completions = self.updates(observation.updates);
        Outcome {
            status: observation.status,
            pending: !observation.status.is_terminal()
                || started
                || observation.uncertain
                || self.job_counts.contains_key(&owner)
                || self.uncertain.contains(&owner),
            output: observation.output,
            completions,
        }
    }

    pub fn message(&mut self, observation: Observation) -> Vec<Completion> {
        if observation.dedup.is_some_and(|key| !self.notices.insert(key)) {
            return Vec::new();
        }
        self.updates(observation.updates)
    }

    fn updates(&mut self, updates: Vec<Update>) -> Vec<Completion> {
        let mut completed = Vec::new();
        for mut update in updates {
            let owner = match update.target {
                Target::Call(id) => {
                    if !update.status.is_terminal() {
                        self.calls.insert(id);
                        continue;
                    }
                    self.calls.remove(&id);
                    self.uncertain.remove(&id);
                    self.jobs.retain(|_, owner| owner != &id);
                    self.job_counts.remove(&id);
                    self.terminal.remove(&id);
                    id
                }
                Target::Job(job) => {
                    if !update.status.is_terminal() {
                        continue;
                    }
                    let Some(owner) = self.jobs.remove(&job) else {
                        continue;
                    };
                    // The last job to finish need not be the one that failed. Retain terminal
                    // evidence while siblings run, and never promote their owner to success.
                    let evidence = self.terminal.entry(owner.clone()).or_insert((Status::Complete, None));
                    if severity(update.status) > severity(evidence.0) {
                        *evidence = (update.status, update.output.take());
                    }
                    let count = self.job_counts.get_mut(&owner).expect("tracked job owner");
                    *count -= 1;
                    if *count != 0 {
                        continue;
                    }
                    self.job_counts.remove(&owner);
                    if self.uncertain.contains(&owner) {
                        continue;
                    }
                    self.calls.remove(&owner);
                    let (status, output) = self.terminal.remove(&owner).expect("terminal job evidence");
                    if severity(status) > severity(update.status) {
                        update.status = status;
                        update.output = output.or_else(|| {
                            Some(
                                match status {
                                    Status::Failed => "background job failed before remaining jobs finished",
                                    Status::Stopped => "background job stopped before remaining jobs finished",
                                    _ => unreachable!(),
                                }
                                .to_owned(),
                            )
                        });
                    }
                    owner
                }
            };
            completed.push(Completion {
                call: owner,
                status: update.status,
                output: update.output,
            });
        }
        completed
    }

    pub fn unsettled(mut self) -> Vec<String> {
        self.calls.extend(self.jobs.into_values());
        self.calls.extend(self.uncertain);
        self.calls.into_iter().collect()
    }
}

fn severity(status: Status) -> u8 {
    match status {
        Status::Pending | Status::Complete => 0,
        Status::Stopped => 1,
        Status::Failed => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_sibling_failure_or_stop_survives_later_success() {
        for status in [Status::Failed, Status::Stopped] {
            let mut state = State::default();
            state.call("launch");
            state.result(
                "launch",
                Observation {
                    started_jobs: vec!["child".into(), "parent".into()],
                    ..Default::default()
                },
            );
            assert!(state
                .message(Observation {
                    updates: vec![Update {
                        target: Target::Job("child".into()),
                        status,
                        output: Some("earlier terminal evidence".into())
                    }],
                    ..Default::default()
                })
                .is_empty());
            let completions = state.message(Observation {
                updates: vec![Update {
                    target: Target::Job("parent".into()),
                    status: Status::Complete,
                    output: Some("later success".into()),
                }],
                ..Default::default()
            });
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].call, "launch");
            assert_eq!(completions[0].status, status);
            assert_eq!(completions[0].output.as_deref(), Some("earlier terminal evidence"));
            assert!(state.unsettled().is_empty());
        }
    }
}
