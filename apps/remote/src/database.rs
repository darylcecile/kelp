use std::path::Path;

use kelp_core::{
    ChangeInfo, MAX_METADATA_BYTES, PROTOCOL, ProjectInfo, Publication, PublicationReceipt,
    Revision, Snapshot, object_id, storage, validate_hash, validate_name,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::Error;

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}

pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let db = storage::open(path)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (name TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS revisions (
            project TEXT NOT NULL REFERENCES projects(name),
            id TEXT NOT NULL,
            change_id TEXT NOT NULL,
            PRIMARY KEY (project, id)
         );
         CREATE TABLE IF NOT EXISTS heads (
            project TEXT NOT NULL,
            change_id TEXT NOT NULL,
            revision TEXT NOT NULL,
            PRIMARY KEY (project, change_id, revision),
            FOREIGN KEY (project, revision) REFERENCES revisions(project, id)
         );
         CREATE TABLE IF NOT EXISTS publications (
            project TEXT NOT NULL REFERENCES projects(name),
            request_id TEXT NOT NULL,
            digest TEXT NOT NULL,
            receipt TEXT NOT NULL,
            PRIMARY KEY (project, request_id)
         );",
    )?;
    Ok(db)
}

pub fn create_project(db: &Connection, name: &str) -> Result<ProjectInfo, Error> {
    validate_name(name).map_err(invalid)?;
    db.execute("INSERT OR IGNORE INTO projects(name) VALUES (?1)", [name])
        .map_err(anyhow::Error::from)?;
    project(db, name)
}

pub fn project(db: &Connection, name: &str) -> Result<ProjectInfo, Error> {
    validate_name(name).map_err(invalid)?;
    let exists: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE name = ?1)",
            [name],
            |r| r.get(0),
        )
        .map_err(anyhow::Error::from)?;
    if !exists {
        return Err(Error::Missing(format!("project {name} does not exist")));
    }
    Ok(ProjectInfo {
        project: name.into(),
        protocol: PROTOCOL.into(),
    })
}

pub fn upload(
    db: &Connection,
    name: &str,
    kind: &str,
    id: &str,
    bytes: &[u8],
) -> Result<(), Error> {
    project(db, name)?;
    validate_hash(id).map_err(invalid)?;
    if !matches!(kind, "blob" | "snapshot") {
        return Err(invalid("only blobs and snapshots can be uploaded"));
    }
    if object_id(kind, bytes) != id {
        return Err(invalid("object bytes do not match the requested ID"));
    }
    if kind == "snapshot" {
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(invalid("snapshot exceeds metadata size limit"));
        }
        let snapshot: Snapshot = serde_json::from_slice(bytes).map_err(invalid)?;
        if serde_json::to_vec(&snapshot).map_err(anyhow::Error::from)? != bytes {
            return Err(invalid("snapshot must use the canonical v0 encoding"));
        }
        storage::verify_snapshot(db, name, &snapshot).map_err(invalid)?;
    }
    storage::put(db, name, kind, bytes)?;
    Ok(())
}

pub fn object(db: &Connection, name: &str, kind: &str, id: &str) -> Result<Vec<u8>, Error> {
    project(db, name)?;
    validate_hash(id).map_err(invalid)?;
    if !matches!(kind, "blob" | "snapshot" | "revision") {
        return Err(invalid("unknown object kind"));
    }
    if !storage::contains(db, name, kind, id)? {
        return Err(Error::Missing(format!("{kind} {id} is not available")));
    }
    Ok(storage::get(db, name, kind, id)?)
}

pub fn change(db: &Connection, name: &str, change: &str) -> Result<ChangeInfo, Error> {
    project(db, name)?;
    validate_name(change).map_err(invalid)?;
    let mut stmt = db
        .prepare(
            "SELECT revision FROM heads WHERE project = ?1 AND change_id = ?2 ORDER BY revision",
        )
        .map_err(anyhow::Error::from)?;
    let heads = stmt
        .query_map(params![name, change], |r| r.get(0))
        .map_err(anyhow::Error::from)?
        .collect::<Result<Vec<String>, _>>()
        .map_err(anyhow::Error::from)?;
    if heads.is_empty() {
        return Err(Error::Missing(format!("change {change} does not exist")));
    }
    Ok(ChangeInfo {
        change: change.into(),
        heads,
    })
}

pub fn changes(db: &Connection, name: &str) -> Result<Vec<ChangeInfo>, Error> {
    project(db, name)?;
    let mut stmt = db
        .prepare("SELECT DISTINCT change_id FROM heads WHERE project = ?1 ORDER BY change_id")
        .map_err(anyhow::Error::from)?;
    let names = stmt
        .query_map([name], |r| r.get(0))
        .map_err(anyhow::Error::from)?
        .collect::<Result<Vec<String>, _>>()
        .map_err(anyhow::Error::from)?;
    names.iter().map(|id| change(db, name, id)).collect()
}

pub fn publish(
    db: &mut Connection,
    name: &str,
    change_id: &str,
    publication: Publication,
) -> Result<PublicationReceipt, Error> {
    project(db, name)?;
    validate_name(&publication.request_id).map_err(invalid)?;
    let revision = &publication.revision;
    revision.validate().map_err(invalid)?;
    if revision.project != name || revision.change != change_id {
        return Err(invalid("revision belongs to a different project or change"));
    }
    let digest = object_id(
        "publication",
        &serde_json::to_vec(&publication).map_err(anyhow::Error::from)?,
    );
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(anyhow::Error::from)?;
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT digest, receipt FROM publications WHERE project = ?1 AND request_id = ?2",
            params![name, publication.request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(anyhow::Error::from)?;
    if let Some((old_digest, receipt)) = previous {
        if old_digest != digest {
            return Err(Error::Conflict(
                "request ID already used for another publication".into(),
            ));
        }
        return Ok(serde_json::from_str(&receipt).map_err(anyhow::Error::from)?);
    }
    for id in [&revision.base_snapshot, &revision.result_snapshot] {
        let snapshot: Snapshot = storage::get_json(&tx, name, "snapshot", id).map_err(invalid)?;
        storage::verify_snapshot(&tx, name, &snapshot).map_err(invalid)?;
    }
    if let Some(parent) = &revision.predecessor {
        let parent: Revision = storage::get_json(&tx, name, "revision", parent).map_err(invalid)?;
        if parent.change != change_id
            || parent.project != name
            || parent.base_snapshot != revision.base_snapshot
        {
            return Err(invalid(
                "predecessor must belong to the same change and base",
            ));
        }
    } else {
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM revisions WHERE project = ?1 AND change_id = ?2)",
                params![name, change_id],
                |r| r.get(0),
            )
            .map_err(anyhow::Error::from)?;
        if exists && !storage::contains(&tx, name, "revision", &revision.id()?)? {
            return Err(Error::Conflict(
                "an existing change requires a predecessor".into(),
            ));
        }
    }
    let id = storage::put_json(&tx, name, "revision", revision)?;
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO revisions(project, id, change_id) VALUES (?1, ?2, ?3)",
            params![name, id, change_id],
        )
        .map_err(anyhow::Error::from)?;
    // Replaying an old revision must not resurrect it as a current head.
    if inserted != 0 {
        if let Some(parent) = &revision.predecessor {
            tx.execute(
                "DELETE FROM heads WHERE project = ?1 AND change_id = ?2 AND revision = ?3",
                params![name, change_id, parent],
            )
            .map_err(anyhow::Error::from)?;
        }
        tx.execute(
            "INSERT INTO heads(project, change_id, revision) VALUES (?1, ?2, ?3)",
            params![name, change_id, id],
        )
        .map_err(anyhow::Error::from)?;
    }
    let receipt = PublicationReceipt {
        request_id: publication.request_id.clone(),
        change: change_id.into(),
        revision: id,
        heads: change(&tx, name, change_id)?.heads,
    };
    tx.execute(
        "INSERT INTO publications(project, request_id, digest, receipt) VALUES (?1, ?2, ?3, ?4)",
        params![
            name,
            publication.request_id,
            digest,
            serde_json::to_string(&receipt).map_err(anyhow::Error::from)?
        ],
    )
    .map_err(anyhow::Error::from)?;
    tx.commit().map_err(anyhow::Error::from)?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelp_core::FileEntry;

    fn snapshot(db: &Connection, bytes: &[u8]) -> anyhow::Result<String> {
        let blob = object_id("blob", bytes);
        upload(db, "demo", "blob", &blob, bytes)?;
        let snapshot = Snapshot {
            files: std::collections::BTreeMap::from([(
                "file.txt".into(),
                FileEntry {
                    blob,
                    size: bytes.len() as u64,
                    executable: false,
                },
            )]),
        };
        let id = snapshot.id()?;
        upload(db, "demo", "snapshot", &id, &serde_json::to_vec(&snapshot)?)?;
        Ok(id)
    }

    #[test]
    fn publications_retain_divergence_and_survive_retries_and_restart() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("remote.sqlite3");
        let mut db = open(&path)?;
        create_project(&db, "demo")?;
        let base = snapshot(&db, b"base")?;
        let first = Publication {
            request_id: "request-1".into(),
            revision: Revision {
                project: "demo".into(),
                change: "change-1".into(),
                predecessor: None,
                base_snapshot: base.clone(),
                result_snapshot: snapshot(&db, b"first")?,
                message: "First attempt".into(),
            },
        };
        let receipt = publish(&mut db, "demo", "change-1", first.clone())?;
        let next = Publication {
            request_id: "request-2".into(),
            revision: Revision {
                predecessor: Some(receipt.revision.clone()),
                result_snapshot: snapshot(&db, b"second")?,
                ..first.revision.clone()
            },
        };
        let next_receipt = publish(&mut db, "demo", "change-1", next.clone())?;
        assert_eq!(
            publish(&mut db, "demo", "change-1", first.clone())?,
            receipt
        );
        assert_eq!(
            change(&db, "demo", "change-1")?.heads,
            std::slice::from_ref(&next_receipt.revision)
        );
        // Even a replay with a new request ID must not resurrect an old head.
        publish(
            &mut db,
            "demo",
            "change-1",
            Publication {
                request_id: "replay".into(),
                ..first.clone()
            },
        )?;
        assert_eq!(
            change(&db, "demo", "change-1")?.heads,
            std::slice::from_ref(&next_receipt.revision)
        );
        let fork = Publication {
            request_id: "request-3".into(),
            revision: Revision {
                result_snapshot: snapshot(&db, b"parallel")?,
                ..next.revision.clone()
            },
        };
        let fork_receipt = publish(&mut db, "demo", "change-1", fork)?;
        assert_eq!(fork_receipt.heads.len(), 2);
        assert!(fork_receipt.heads.contains(&next_receipt.revision));
        assert!(
            publish(
                &mut db,
                "demo",
                "change-1",
                Publication {
                    request_id: first.request_id,
                    ..next
                }
            )
            .is_err()
        );
        drop(db);
        let db = open(&path)?;
        assert_eq!(change(&db, "demo", "change-1")?.heads, fork_receipt.heads);
        Ok(())
    }

    #[test]
    fn incomplete_or_cross_project_publications_never_become_visible() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let mut db = open(&directory.path().join("remote.sqlite3"))?;
        create_project(&db, "demo")?;
        create_project(&db, "other")?;
        let base = snapshot(&db, b"base")?;
        let revision = Revision {
            project: "demo".into(),
            change: "c".into(),
            predecessor: None,
            base_snapshot: base.clone(),
            result_snapshot: object_id("snapshot", b"missing"),
            message: "Missing contents".into(),
        };
        assert!(
            publish(
                &mut db,
                "demo",
                "c",
                Publication {
                    request_id: "req".into(),
                    revision
                }
            )
            .is_err()
        );
        assert!(changes(&db, "demo")?.is_empty());
        assert!(object(&db, "other", "snapshot", &base).is_err());
        assert!(upload(&db, "demo", "blob", &object_id("blob", b"good"), b"bad").is_err());
        Ok(())
    }
}
