//! Bounded JSONL staging: serialize successfully before opening the destination.
use super::bytes::ByteStage;
use crate::publish::{Claim, Set};
use anyhow::Result;
use serde::Serialize;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

pub(crate) struct JsonlStage {
    output: BufWriter<ByteStage>,
    records: usize,
    /// Some once a push failed (or was interrupted): the stage is poisoned, and this names the
    /// record and cause for every later call.
    failed: Option<String>,
}
impl JsonlStage {
    pub(crate) fn new() -> Self {
        Self {
            output: BufWriter::with_capacity(super::IO_CHUNK, ByteStage::new()),
            records: 0,
            failed: None,
        }
    }
    pub(crate) fn record_count(&self) -> usize {
        self.records
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.records == 0
    }
    pub(crate) fn push(&mut self, value: impl Serialize) -> Result<()> {
        self.push_with(|writer| serde_json::to_writer(writer, &value).map_err(Into::into))
    }
    pub(crate) fn push_with(&mut self, produce: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
        if let Some(failure) = &self.failed {
            anyhow::bail!("JSONL stage already failed at {failure}");
        }
        // Poisoned until the record is whole, so a push that never returns leaves it failed.
        self.failed = Some(format!("record {} (interrupted)", self.records));
        let pushed = produce(&mut self.output).and_then(|()| Ok(self.output.write_all(b"\n")?));
        match pushed {
            Ok(()) => {
                self.records += 1;
                self.failed = None;
                Ok(())
            }
            Err(error) => {
                self.failed = Some(format!("record {}: {error:#}", self.records));
                Err(error)
            }
        }
    }
    fn refuse_if_failed(&self, what: &str) -> Result<()> {
        match &self.failed {
            Some(failure) => anyhow::bail!("cannot {what} a failed JSONL stage (failed at {failure})"),
            None => Ok(()),
        }
    }
    pub(crate) fn into_source(self) -> Result<super::bytes::ByteSource> {
        self.refuse_if_failed("consume")?;
        Ok(self.output.into_inner().map_err(|e| e.into_error())?.finish()?)
    }
    pub(crate) fn publish(self, path: &Path, claim: Claim) -> Result<()> {
        publish_stage(self.into_stage()?, path, claim)
    }
    /// [`Self::publish`], running `check` on the staged file before it claims `path`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn publish_checked(
        self,
        path: &Path,
        claim: Claim,
        check: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        publish_stage_checked(self.into_stage()?, path, claim, check)
    }
    /// Stage the records for `path` as one member of `set`.
    #[allow(dead_code)]
    pub(crate) fn stage_into(self, set: &mut Set, path: &Path, claim: Claim) -> Result<()> {
        stage_into(set, self.into_stage()?, path, claim)
    }
    fn into_stage(self) -> Result<ByteStage> {
        self.refuse_if_failed("publish")?;
        Ok(self.output.into_inner().map_err(|e| e.into_error())?)
    }
}

/// Publish fully produced bytes through [`crate::publish`], never through an existing inode;
/// `Claim::Replace` replaces the destination name with the staged file.
/// Checked serialization must complete before calling.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn publish_stage(stage: ByteStage, path: &Path, claim: Claim) -> Result<()> {
    publish_stage_checked(stage, path, claim, |_| Ok(()))
}

/// [`publish_stage`], running `check` on the staged file before it claims `path`; on an error the
/// staged file is removed and `path` keeps what it held.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn publish_stage_checked(
    stage: ByteStage,
    path: &Path,
    claim: Claim,
    check: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let source = stage.finish()?;
    let reader = BufReader::with_capacity(super::COPY_CHUNK, source.reader()?);
    let staged = crate::publish::stage_reader(path, "stage", reader, claim)?;
    check(staged.path())?;
    staged.publish(path, claim)?;
    Ok(())
}

/// [`publish_stage`] as one member of `set`.
#[allow(dead_code)]
pub(crate) fn stage_into(set: &mut Set, stage: ByteStage, path: &Path, claim: Claim) -> Result<()> {
    let source = stage.finish()?;
    let reader = BufReader::with_capacity(super::COPY_CHUNK, source.reader()?);
    set.stage(path, "stage", reader, claim)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_spilled_jsonl_and_failed_publication() -> Result<()> {
        let dir = cv_testkit::World::new("jsonl-stage-spill");
        let path = dir.base.join("records.jsonl");
        let value = serde_json::json!({"escaped": "a\"\\\nλ".repeat(300_000), "n": 1.25});
        let mut stage = JsonlStage::new();
        stage.push(&value)?;
        stage.push(serde_json::Value::Null)?;
        stage.publish(&path, Claim::New)?;
        assert_eq!(
            std::fs::read(&path)?,
            format!("{}\nnull\n", serde_json::to_string(&value)?).as_bytes()
        );
        struct Failure;
        impl Serialize for Failure {
            fn serialize<S: serde::Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("synthetic failure"))
            }
        }
        let before = std::fs::read(&path)?;
        let mut stage = JsonlStage::new();
        stage.push(serde_json::json!({"valid": true}))?;
        assert!(stage.push(Failure).is_err());
        // Every later refusal names the record that failed and why.
        let again = stage.push(serde_json::Value::Null).unwrap_err().to_string();
        assert_eq!(again, "JSONL stage already failed at record 1: synthetic failure");
        let refused = stage.publish(&path, Claim::Replace).unwrap_err().to_string();
        assert_eq!(
            refused,
            "cannot publish a failed JSONL stage (failed at record 1: synthetic failure)"
        );
        assert_eq!(std::fs::read(&path)?, before);
        let mut stage = JsonlStage::new();
        assert!(stage
            .push_with(|writer| {
                writer.write_all(b"{\"partial\":")?;
                anyhow::bail!("checked payload source failed")
            })
            .is_err());
        assert!(stage.publish(&path, Claim::Replace).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }
    /// The check sees the staged bytes before the name does; when it fails, the destination keeps
    /// what it held (nothing, or the replaced file) and no temporary is left (a Claude emit's
    /// resumability check rides this).
    #[test]
    fn a_failed_check_publishes_nothing() -> Result<()> {
        let world = cv_testkit::World::new("jsonl-stage-check");
        let dir = world.base.join("out");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("records.jsonl");
        for prior in [None, Some(b"prior\n".as_slice())] {
            if let Some(bytes) = prior {
                std::fs::write(&path, bytes)?;
            }
            let mut stage = JsonlStage::new();
            stage.push(serde_json::json!({"n": 1}))?;
            let err = stage
                .publish_checked(&path, Claim::Replace, |staged| {
                    assert_eq!(std::fs::read(staged)?, b"{\"n\":1}\n");
                    anyhow::bail!("not resumable")
                })
                .unwrap_err();
            assert_eq!(err.to_string(), "not resumable");
            assert_eq!(std::fs::read(&path).ok().as_deref(), prior);
            assert_eq!(std::fs::read_dir(&dir)?.count(), usize::from(prior.is_some()));
        }
        Ok(())
    }
    /// A replaced name gets a new file: the old inode, its other hard links and a symlink's
    /// target keep their bytes; the replaced file's permissions carry over; New refuses.
    #[cfg(unix)]
    #[test]
    fn replaces_the_name_never_the_linked_file() -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = cv_testkit::World::new("jsonl-stage-links");
        let path = dir.base.join("original");
        let hard = dir.base.join("hard");
        let sym = dir.base.join("sym");
        std::fs::write(&path, "before")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
        std::fs::hard_link(&path, &hard)?;
        symlink(&path, &sym)?;
        let staged = || -> Result<JsonlStage> {
            let mut stage = JsonlStage::new();
            stage.push(42)?;
            Ok(stage)
        };
        let error = staged()?.publish(&sym, Claim::New).unwrap_err();
        assert!(error.downcast_ref::<crate::publish::Exists>().is_some(), "{error:#}");
        staged()?.publish(&sym, Claim::Replace)?;
        assert_eq!(std::fs::read(&sym)?, b"42\n");
        assert!(!std::fs::symlink_metadata(&sym)?.file_type().is_symlink());
        assert_eq!(std::fs::metadata(&sym)?.permissions().mode() & 0o777, 0o640);
        staged()?.publish(&path, Claim::Replace)?;
        assert_eq!(std::fs::read(&path)?, b"42\n");
        assert_eq!(std::fs::read(&hard)?, b"before");
        Ok(())
    }
}
