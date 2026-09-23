use std::{collections::BTreeSet, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use kelp_core::{
    ApiError, ChangeInfo, PROTOCOL, ProjectInfo, Publication, PublicationReceipt, Revision,
    Snapshot, object_id, storage, validate_name,
};
use reqwest::{
    StatusCode, Url,
    blocking::{Client, RequestBuilder, Response},
};
use serde::de::DeserializeOwned;

use crate::workspace::{Workspace, materialize};

pub struct Remote {
    client: Client,
    project_url: String,
    token: String,
}

pub fn normalize_url(value: &str) -> Result<String> {
    let url = Url::parse(value).context("remote must be an HTTP or HTTPS URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "remote must be an HTTP or HTTPS URL"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "remote URL must not contain credentials, query parameters, or a fragment"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

impl Remote {
    pub fn new(url: &str, project: &str, token: String) -> Result<Self> {
        validate_name(project)?;
        ensure!(
            !token.trim().is_empty(),
            "set KELP_TOKEN to the remote's access token"
        );
        let base = normalize_url(url)?;
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            project_url: format!("{base}/v0/projects/{project}"),
            token,
        })
    }

    fn send(&self, request: RequestBuilder) -> Result<Response> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .context("cannot reach the remote; local work is retained")?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text()?;
        let message = serde_json::from_str::<ApiError>(&body)
            .map(|e| format!("{}: {}", e.code, e.message))
            .unwrap_or(body);
        anyhow::bail!("remote returned {status}: {message}")
    }

    pub fn project(&self, create: bool) -> Result<ProjectInfo> {
        let request = if create {
            self.client.put(&self.project_url)
        } else {
            self.client.get(&self.project_url)
        };
        let info: ProjectInfo = self.send(request)?.json()?;
        ensure!(
            info.protocol == PROTOCOL,
            "unsupported remote protocol {}",
            info.protocol
        );
        Ok(info)
    }

    pub fn changes(&self) -> Result<Vec<ChangeInfo>> {
        Ok(self
            .send(self.client.get(format!("{}/changes", self.project_url)))?
            .json()?)
    }

    pub fn change(&self, change: &str) -> Result<ChangeInfo> {
        validate_name(change)?;
        Ok(self
            .send(
                self.client
                    .get(format!("{}/changes/{change}", self.project_url)),
            )?
            .json()?)
    }

    fn object_url(&self, kind: &str, id: &str) -> String {
        format!("{}/objects/{kind}/{id}", self.project_url)
    }

    fn has(&self, kind: &str, id: &str) -> Result<bool> {
        let response = self
            .client
            .head(self.object_url(kind, id))
            .bearer_auth(&self.token)
            .send()?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(false),
            status if status.is_success() => Ok(true),
            status => anyhow::bail!("remote returned {status} while checking {kind} {id}"),
        }
    }

    pub fn download(&self, kind: &str, id: &str) -> Result<Vec<u8>> {
        kelp_core::validate_hash(id)?;
        let bytes = self
            .send(self.client.get(self.object_url(kind, id)))?
            .bytes()?
            .to_vec();
        ensure!(
            object_id(kind, &bytes) == id,
            "remote returned corrupt {kind} object {id}"
        );
        Ok(bytes)
    }

    fn json<T: DeserializeOwned>(&self, kind: &str, id: &str) -> Result<T> {
        Ok(serde_json::from_slice(&self.download(kind, id)?)?)
    }

    fn upload_snapshot(
        &self,
        workspace: &Workspace,
        id: &str,
        sent: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !sent.insert(id.into()) || self.has("snapshot", id)? {
            return Ok(());
        }
        let bytes = storage::get(&workspace.db, &workspace.state.project, "snapshot", id)?;
        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        for file in snapshot.files.values() {
            if sent.insert(file.blob.clone()) && !self.has("blob", &file.blob)? {
                let bytes =
                    storage::get(&workspace.db, &workspace.state.project, "blob", &file.blob)?;
                self.send(
                    self.client
                        .put(self.object_url("blob", &file.blob))
                        .body(bytes),
                )?;
            }
        }
        self.send(self.client.put(self.object_url("snapshot", id)).body(bytes))?;
        Ok(())
    }

    fn publish_one(
        &self,
        workspace: &Workspace,
        publication: &Publication,
        sent: &mut BTreeSet<String>,
    ) -> Result<PublicationReceipt> {
        for id in [
            &publication.revision.base_snapshot,
            &publication.revision.result_snapshot,
        ] {
            self.upload_snapshot(workspace, id, sent)?;
        }
        let receipt: PublicationReceipt = self
            .send(
                self.client
                    .post(format!(
                        "{}/changes/{}/publications",
                        self.project_url, publication.revision.change
                    ))
                    .json(publication),
            )?
            .json()?;
        ensure!(
            receipt.request_id == publication.request_id
                && receipt.revision == publication.revision.id()?
                && receipt.change == publication.revision.change,
            "remote publication receipt does not match the request"
        );
        Ok(receipt)
    }

    pub fn publish(
        &self,
        workspace: &Workspace,
        publication: &Publication,
    ) -> Result<PublicationReceipt> {
        self.project(true)?;
        let mut missing = Vec::new();
        let mut parent = publication.revision.predecessor.clone();
        while let Some(id) = parent {
            if self.has("revision", &id)? {
                break;
            }
            let revision: Revision =
                storage::get_json(&workspace.db, &workspace.state.project, "revision", &id)?;
            parent = revision.predecessor.clone();
            missing.push(Publication {
                request_id: id,
                revision,
            });
        }
        let mut sent = BTreeSet::new();
        for previous in missing.iter().rev() {
            self.publish_one(workspace, previous, &mut sent)?;
        }
        self.publish_one(workspace, publication, &mut sent)
    }

    pub fn open(
        &self,
        url: &str,
        project: &str,
        change: &str,
        selected: Option<&str>,
        destination: &Path,
    ) -> Result<Workspace> {
        self.project(false)?;
        let info = self.change(change)?;
        let id = match selected {
            Some(id) => id.to_owned(),
            None => {
                ensure!(
                    info.heads.len() == 1,
                    "change has divergent revisions; choose one with --revision"
                );
                info.heads[0].clone()
            }
        };
        let revision: Revision = self.json("revision", &id)?;
        revision.validate()?;
        ensure!(
            revision.project == project && revision.change == change,
            "revision belongs to another project or change"
        );
        ensure!(
            std::fs::symlink_metadata(destination).is_err(),
            "destination already exists"
        );
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let staging = tempfile::tempdir_in(parent)?;
        let staging_db = storage::open(&staging.path().join("transfer.sqlite3"))?;
        for snapshot_id in [&revision.base_snapshot, &revision.result_snapshot] {
            let snapshot: Snapshot = self.json("snapshot", snapshot_id)?;
            snapshot.validate()?;
            for file in snapshot.files.values() {
                if !storage::contains(&staging_db, project, "blob", &file.blob)? {
                    storage::put(
                        &staging_db,
                        project,
                        "blob",
                        &self.download("blob", &file.blob)?,
                    )?;
                }
            }
            storage::verify_snapshot(&staging_db, project, &snapshot)?;
            ensure!(
                storage::put_json(&staging_db, project, "snapshot", &snapshot)? == *snapshot_id,
                "noncanonical snapshot"
            );
        }
        let snapshot: Snapshot =
            storage::get_json(&staging_db, project, "snapshot", &revision.result_snapshot)?;
        let checkout = staging.path().join("checkout");
        materialize(&staging_db, project, &snapshot, &checkout)?;
        let mut workspace = Workspace::init(&checkout, project, Some(normalize_url(url)?))?;
        // Preserve tracked files even when their paths match an imported ignore rule.
        workspace.state.tracked = snapshot.files.keys().cloned().collect();
        let mut stmt =
            staging_db.prepare("SELECT kind, bytes FROM objects WHERE namespace = ?1")?;
        for row in stmt.query_map([project], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })? {
            let (kind, bytes) = row?;
            storage::put(&workspace.db, project, &kind, &bytes)?;
        }
        storage::put_json(&workspace.db, project, "revision", &revision)?;
        workspace.state.base_snapshot = revision.base_snapshot;
        workspace.state.change = Some(change.into());
        workspace.state.head = Some(id);
        workspace.state.message = Some(revision.message);
        workspace.save_state()?;
        workspace.capture(Some("Opened remote change"))?;
        drop(workspace);
        std::fs::rename(&checkout, destination)?;
        Workspace::open(destination)
    }
}
