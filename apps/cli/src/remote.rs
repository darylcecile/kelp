use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use kelp_core::{
    ApiError, Snapshot, storage,
    transactions::{Graph, PROTOCOL, Project, Receipt, SyncPage, SyncRequest, Transaction, View},
    transfer::{self, Info, Key, Object},
    validate_name,
};
use reqwest::{
    Url,
    blocking::{Client, RequestBuilder, Response},
};

use crate::index::Projection;
use crate::merge;
use crate::selection;
use crate::workspace::{Resolution, Workspace};
use kelp_core::{EntryKind, FileEntry, content};

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

pub fn project_location(value: &str) -> Result<(String, String)> {
    let normalized = normalize_url(value)?;
    let mut url = Url::parse(&normalized)?;
    let (prefix, project) = url
        .path()
        .trim_end_matches('/')
        .rsplit_once('/')
        .context("include the project name, for example https://code.example.com/my-project")?;
    validate_name(project).context("include a valid project name at the end of the remote URL")?;
    let (prefix, project) = (prefix.to_owned(), project.to_owned());
    url.set_path(&prefix);
    Ok((url.as_str().trim_end_matches('/').into(), project))
}

pub fn project_url(base: &str, project: &str) -> String {
    format!("{}/{project}", base.trim_end_matches('/'))
}

impl Remote {
    pub fn new(url: &str, project: &str, token: String) -> Result<Self> {
        validate_name(project)?;
        ensure!(
            !token.trim().is_empty(),
            "set KELP_TOKEN to the remote's access token"
        );
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            project_url: format!("{}/v1/projects/{project}", normalize_url(url)?),
            token,
        })
    }

    fn send(&self, request: RequestBuilder) -> Result<Response> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .context("cannot reach the remote; committed work remains local")?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text()?;
        let message = serde_json::from_str::<ApiError>(&body)
            .map(|error| format!("{}: {}", error.code, error.message))
            .unwrap_or(body);
        anyhow::bail!("remote returned {status}: {message}")
    }

    pub fn project(&self, create: bool) -> Result<Project> {
        let request = if create {
            self.client.put(&self.project_url)
        } else {
            self.client.get(&self.project_url)
        };
        let project: Project = self
            .send(request)
            .context("the remote must support Kelp transaction synchronization (kelp/1)")?
            .json()?;
        ensure!(
            project.protocol == PROTOCOL,
            "unsupported remote protocol {}",
            project.protocol
        );
        Ok(project)
    }

    fn info(&self, keys: &[Key]) -> Result<Vec<Info>> {
        let mut result = Vec::new();
        for group in keys.chunks(transfer::MAX_OBJECTS) {
            let reply: Vec<Info> = self
                .send(
                    self.client
                        .post(format!("{}/objects/info", self.project_url))
                        .json(&transfer::Request {
                            objects: group.to_vec(),
                        }),
                )?
                .json()?;
            ensure!(
                reply.len() == group.len()
                    && reply
                        .iter()
                        .map(|info| &info.object)
                        .collect::<BTreeSet<_>>()
                        == group.iter().collect(),
                "invalid object inventory response"
            );
            result.extend(reply);
        }
        Ok(result)
    }

    fn download(&self, workspace: &Workspace, keys: &[Key]) -> Result<()> {
        let infos = self.info(keys)?;
        let mut group = Vec::new();
        let mut total = 4;
        for info in infos {
            let size = usize::try_from(info.size.context("remote object is missing")?)?;
            ensure!(
                size <= info.object.limit(),
                "remote object exceeds size limit"
            );
            if group.len() == transfer::MAX_OBJECTS || total + size + 69 > transfer::MAX_PACK_BYTES
            {
                self.download_group(workspace, &group)?;
                group.clear();
                total = 4;
            }
            total += size + 69;
            group.push(info.object);
        }
        if !group.is_empty() {
            self.download_group(workspace, &group)?;
        }
        Ok(())
    }

    fn download_group(&self, workspace: &Workspace, keys: &[Key]) -> Result<()> {
        let bytes = self
            .send(
                self.client
                    .post(format!("{}/objects/download", self.project_url))
                    .json(&transfer::Request {
                        objects: keys.to_vec(),
                    }),
            )?
            .bytes()?;
        let objects = transfer::decode(&bytes)?;
        ensure!(
            objects
                .iter()
                .map(|object| &object.key)
                .collect::<BTreeSet<_>>()
                == keys.iter().collect(),
            "remote object batch does not match the request"
        );
        let transaction = workspace.db.unchecked_transaction()?;
        for object in objects {
            storage::put(
                &transaction,
                &workspace.state.project,
                &object.key.kind,
                &object.bytes,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn upload(&self, workspace: &Workspace, keys: &[Key], replicas: usize) -> Result<()> {
        let infos = self.info(keys)?;
        let mut objects = Vec::new();
        let mut total = 4;
        for info in infos
            .into_iter()
            .filter(|info| info.size.is_none() || info.copies < replicas)
        {
            let bytes = storage::get(
                &workspace.db,
                &workspace.state.project,
                &info.object.kind,
                &info.object.id,
            ).with_context(|| "required content is not cached; a partial checkout can push new commits to its original remote, but cannot copy missing outside-path history to a different remote")?;
            if objects.len() == transfer::MAX_OBJECTS
                || total + bytes.len() + 69 > transfer::MAX_PACK_BYTES
            {
                self.upload_group(&objects)?;
                objects.clear();
                total = 4;
            }
            total += bytes.len() + 69;
            objects.push(Object {
                key: info.object,
                bytes,
            });
        }
        if !objects.is_empty() {
            self.upload_group(&objects)?;
        }
        Ok(())
    }

    fn upload_group(&self, objects: &[Object]) -> Result<()> {
        self.send(
            self.client
                .post(format!("{}/objects/upload", self.project_url))
                .body(transfer::encode(objects)?),
        )?;
        Ok(())
    }

    /// Transfer the immutable outbox, never the working directory.
    pub fn push(&self, workspace: &mut Workspace) -> Result<usize> {
        let pins = workspace.pins()?;
        if workspace.state.outbox.is_empty() && pins.is_empty() {
            return Ok(0);
        }
        let project = self.project(workspace.state.layout.is_none())?;
        if workspace.state.layout.as_ref() != Some(&project.layout) {
            workspace.state.cursors.clear();
        }
        workspace.state.layout = Some(project.layout);
        workspace.save_state()?;
        let mut missing = Graph::default();
        let mut todo = workspace.state.outbox.clone();
        for pin in &pins {
            todo.extend(pin.roots.iter().cloned());
        }
        let mut known = BTreeSet::new();
        let count = workspace.state.outbox.len();
        while !todo.is_empty() {
            let ids = std::mem::take(&mut todo);
            let keys: Vec<_> = ids
                .into_iter()
                .map(|id| Key {
                    kind: "transaction".into(),
                    id,
                })
                .collect();
            for info in self.info(&keys)? {
                let id = info.object.id;
                if info.size.is_some() && info.copies >= project.replicas {
                    known.insert(id);
                    continue;
                }
                let transaction = workspace.transaction(&id)?;
                for parent in transaction.dependencies() {
                    if !missing.transactions.contains_key(&parent) && !known.contains(&parent) {
                        todo.insert(parent);
                    }
                }
                missing.transactions.insert(id, transaction);
            }
            todo.retain(|id| !missing.transactions.contains_key(id) && !known.contains(id));
        }
        let blobs: BTreeSet<_> = missing
            .transactions
            .values()
            .flat_map(|transaction| {
                transaction
                    .edits
                    .values()
                    .filter_map(|edit| edit.value.as_ref())
                    .chain(transaction.provenance.as_ref())
            })
            .map(|value| content::objects(&workspace.db, &workspace.state.project, value))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .map(|id| Key {
                kind: "blob".into(),
                id,
            })
            .collect();
        self.upload(
            workspace,
            &blobs.into_iter().collect::<Vec<_>>(),
            project.replicas,
        )?;
        for id in &known {
            workspace.acknowledge(id)?;
        }
        for id in missing.ordered_after(&known)? {
            let transaction = &missing.transactions[&id];
            let receipt: Receipt = self
                .send(
                    self.client
                        .post(format!("{}/transactions", self.project_url))
                        .json(transaction),
                )?
                .json()?;
            ensure!(
                receipt.transaction == id,
                "remote receipt does not match the committed transaction"
            );
            workspace.acknowledge(&id)?;
        }
        for pin in &pins {
            self.send(
                self.client
                    .post(format!("{}/pins", self.project_url))
                    .json(pin),
            )?;
        }
        Ok(count + pins.len())
    }

    fn fetch(
        &self,
        workspace: &Workspace,
        project: &Project,
    ) -> Result<(Graph, Projection, Vec<i64>, BTreeSet<String>)> {
        let mut cursors = if workspace.state.layout.as_ref() == Some(&project.layout) {
            workspace.state.cursors.clone()
        } else {
            Vec::new()
        };
        let mut graph = Graph::default();
        let mut seen = BTreeSet::new();
        loop {
            let page: SyncPage =
                self.send(self.client.post(format!("{}/sync", self.project_url)).json(
                    &SyncRequest {
                        cursors,
                        paths: workspace.state.paths.clone(),
                    },
                ))?
                .json()?;
            let mut todo = page.transactions;
            while !todo.is_empty() {
                let ids: Vec<_> = std::mem::take(&mut todo)
                    .into_iter()
                    .filter(|id| seen.insert(id.clone()))
                    .collect();
                let mut keys = Vec::new();
                for id in &ids {
                    if !workspace.state.transactions.contains(id)
                        && !storage::contains(
                            &workspace.db,
                            &workspace.state.project,
                            "transaction",
                            id,
                        )?
                    {
                        keys.push(Key {
                            kind: "transaction".into(),
                            id: id.clone(),
                        });
                    }
                }
                self.download(workspace, &keys)?;
                let mut files = Vec::new();
                for id in ids {
                    if workspace.state.transactions.contains(&id) {
                        continue;
                    }
                    let transaction: Transaction = workspace.transaction(&id)?;
                    transaction.validate()?;
                    ensure!(transaction.id()? == id, "noncanonical transaction");
                    todo.extend(transaction.dependencies().difference(&seen).cloned());
                    files.extend(transaction.provenance.iter().cloned());
                    for value in transaction
                        .edits
                        .iter()
                        .filter(|(path, _)| workspace.includes(path))
                        .filter_map(|(_, edit)| edit.value.as_ref())
                    {
                        files.push(value.clone());
                    }
                    graph.transactions.insert(id, transaction);
                }
                self.download_files(workspace, files)?;
            }
            cursors = page.cursors;
            if !page.more {
                break;
            }
        }
        let projection = workspace.extend_projection(&graph)?;
        Ok((graph, projection, cursors, seen))
    }

    fn download_files(&self, workspace: &Workspace, mut files: Vec<FileEntry>) -> Result<()> {
        let mut seen = BTreeSet::new();
        while !files.is_empty() {
            let pending: Vec<_> = std::mem::take(&mut files)
                .into_iter()
                .filter(|entry| seen.insert((entry.blob.clone(), entry.size, entry.kind)))
                .collect();
            let mut keys = BTreeSet::new();
            for entry in &pending {
                entry.validate()?;
                if !storage::contains(&workspace.db, &workspace.state.project, "blob", &entry.blob)?
                {
                    keys.insert(Key {
                        kind: "blob".into(),
                        id: entry.blob.clone(),
                    });
                }
            }
            self.download(workspace, &keys.into_iter().collect::<Vec<_>>())?;
            for entry in pending {
                if entry.kind == EntryKind::Chunked {
                    files.extend(content::children(
                        &entry,
                        &storage::get(
                            &workspace.db,
                            &workspace.state.project,
                            "blob",
                            &entry.blob,
                        )?,
                    )?);
                } else {
                    ensure!(
                        storage::size(
                            &workspace.db,
                            &workspace.state.project,
                            "blob",
                            &entry.blob
                        )? == Some(entry.size),
                        "remote file length mismatch"
                    );
                }
            }
        }
        Ok(())
    }

    fn fetch_pins(&self, workspace: &Workspace) -> Result<()> {
        if !workspace.state.paths.is_empty() {
            return Ok(());
        }
        let mut after = String::new();
        loop {
            let reply = self
                .client
                .get(format!("{}/pins", self.project_url))
                .query(&[("after", &after)])
                .bearer_auth(&self.token)
                .send()?;
            if reply.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(());
            }
            let pins: Vec<kelp_core::transactions::Pin> = reply.error_for_status()?.json()?;
            if pins.is_empty() {
                return Ok(());
            }
            for pin in pins {
                ensure!(
                    workspace.graph_from_roots(&pin.roots)?.view()?.snapshot()? == pin.snapshot,
                    "remote tag does not match its committed roots"
                );
                after = pin.id()?;
                workspace.remember_pin(&pin)?;
            }
        }
    }

    pub fn clone_project(
        &self,
        base: &str,
        project_name: &str,
        destination: &Path,
    ) -> Result<Workspace> {
        self.clone_paths(base, project_name, destination, Vec::new())
    }

    pub fn clone_paths(
        &self,
        base: &str,
        project_name: &str,
        destination: &Path,
        paths: Vec<String>,
    ) -> Result<Workspace> {
        let paths = selection::normalize(paths)?;
        let project = self.project(false)?;
        ensure!(
            std::fs::symlink_metadata(destination).is_err(),
            "destination already exists"
        );
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let staging = tempfile::tempdir_in(parent)?;
        let checkout = staging.path().join("checkout");
        let mut workspace = Workspace::init(&checkout, project_name, Some(normalize_url(base)?))?;
        workspace.state.paths = paths;
        if !workspace.state.paths.is_empty() {
            workspace.state.version = 2;
        }
        let (graph, projection, cursors, _) = self.fetch(&workspace, &project)?;
        let view = auto_merge(&workspace, &projection.view, &graph)?;
        let snapshot = workspace.checkout_view(&view)?.snapshot().context("the selected paths contain overlapping conflicts; resolve them in an existing checkout before cloning these paths")?;
        workspace.apply_snapshot(&Snapshot::default(), &snapshot)?;
        workspace.state.transactions = graph.transactions.keys().cloned().collect();
        workspace.cache_projection(&projection)?;
        workspace.state.base_snapshot =
            storage::put_json(&workspace.db, project_name, "snapshot", &snapshot)?;
        workspace.state.tracked = snapshot.files.keys().cloned().collect();
        workspace.state.layout = Some(project.layout);
        workspace.state.cursors = cursors;
        workspace.save_state()?;
        workspace.remember_snapshot(
            workspace.state.base_snapshot.clone(),
            Some(&format!("Cloned {project_name}")),
        )?;
        self.fetch_pins(&workspace)?;
        drop(workspace);
        std::fs::rename(checkout, destination)?;
        Workspace::open(destination)
    }

    pub fn pull(&self, workspace: &mut Workspace, resolution: Resolution) -> Result<bool> {
        self.pull_paths(workspace, resolution, Vec::new())
    }

    pub fn pull_paths(
        &self,
        workspace: &mut Workspace,
        resolution: Resolution,
        paths: Vec<String>,
    ) -> Result<bool> {
        let previous_state = workspace.state.clone();
        let previous = workspace.state.paths.clone();
        let requested = !paths.is_empty();
        let mut selected = selection::normalize(paths)?;
        if !previous.is_empty() && !selected.is_empty() {
            selected.extend(previous.clone());
            selected = selection::normalize(selected)?;
        }
        // No option retains the current scope; an explicit dot expands it fully.
        let expanded = requested && !previous.is_empty() && selected != previous;
        if expanded {
            workspace.state.paths = selected;
            workspace.state.cursors.clear();
        }
        let result = self.pull_selected(workspace, resolution, expanded);
        if result.is_err() {
            workspace.state = previous_state;
            workspace.save_state()?;
        }
        result
    }

    fn pull_selected(
        &self,
        workspace: &mut Workspace,
        resolution: Resolution,
        expanded: bool,
    ) -> Result<bool> {
        let project = self.project(false)?;
        if expanded {
            let history = workspace.graph()?;
            let files = history
                .transactions
                .values()
                .flat_map(|transaction| transaction.edits.iter())
                .filter(|(path, _)| workspace.includes(path))
                .filter_map(|(_, edit)| edit.value.as_ref())
                .cloned()
                .collect();
            self.download_files(workspace, files)?;
        }
        let (graph, projection, cursors, seen) = self.fetch(workspace, &project)?;
        if graph.transactions.is_empty() && !expanded {
            workspace.state.outbox = workspace.state.outbox.difference(&seen).cloned().collect();
            workspace.state.cursors = cursors;
            workspace.state.layout = Some(project.layout);
            workspace.save_state()?;
            self.fetch_pins(workspace)?;
            return Ok(false);
        }
        let base = workspace.baseline()?;
        let view = auto_merge(workspace, &projection.view, &graph)?;
        let incoming = select_snapshot(
            &workspace.checkout_view(&view)?,
            &workspace.state.transactions,
            &base,
            resolution,
        )?;
        let current = workspace.current_snapshot()?;
        let merged = merge::working(workspace, &base, &current, &incoming, resolution)?;
        let backup = workspace.capture(Some("Before pulling commits"))?;
        ensure!(
            workspace.checkpoint_snapshot(backup.id)? == current,
            "files changed while preparing the pull; try again"
        );
        workspace.apply_snapshot(&current, &merged)?;
        workspace
            .state
            .transactions
            .extend(graph.transactions.keys().cloned());
        workspace.cache_projection(&projection)?;
        workspace.state.outbox = workspace.state.outbox.difference(&seen).cloned().collect();
        workspace.state.base_snapshot = storage::put_json(
            &workspace.db,
            &workspace.state.project,
            "snapshot",
            &incoming,
        )?;
        workspace.state.tracked = merged.files.keys().cloned().collect();
        workspace.state.cursors = cursors;
        workspace.state.layout = Some(project.layout);
        workspace.save_state()?;
        workspace.capture(Some("Pulled commits"))?;
        self.fetch_pins(workspace)?;
        Ok(true)
    }
}

fn auto_merge(workspace: &Workspace, view: &View, incoming: &Graph) -> Result<View> {
    let conflicts: Vec<_> = view
        .conflicts()
        .into_iter()
        .filter(|path| workspace.includes(path))
        .collect();
    let mut resolved = view.clone();
    if conflicts.is_empty() {
        return Ok(resolved);
    }
    let mut graph = workspace.graph()?;
    graph.transactions.extend(incoming.transactions.clone());
    for path in conflicts {
        let heads = &view.files[&path];
        if heads.len() > 1
            && let Some(entry) = merge::heads(workspace, &graph, &path, heads)?
        {
            resolved.files.insert(
                path,
                heads
                    .keys()
                    .map(|id| (id.clone(), Some(entry.clone())))
                    .collect(),
            );
        }
    }
    Ok(resolved)
}

fn select_snapshot(
    view: &View,
    previous: &BTreeSet<String>,
    local: &Snapshot,
    resolution: Resolution,
) -> Result<Snapshot> {
    let conflicts = view.conflicts();
    if conflicts.is_empty() {
        return view.snapshot();
    }
    ensure!(
        !matches!(resolution, Resolution::Stop),
        "concurrent commits conflict in: {}. Both versions are retained. Use `kelp pull --keep-local` or `kelp pull --keep-remote`, then commit the resolution",
        conflicts.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    let mut files = BTreeMap::new();
    for (path, heads) in &view.files {
        let value = if !conflicts.contains(path) {
            heads.values().next().cloned().flatten()
        } else if matches!(resolution, Resolution::Local) {
            local.files.get(path).cloned()
        } else {
            let values: Vec<_> = heads
                .iter()
                .filter(|(id, _)| !previous.contains(*id))
                .map(|(_, value)| value)
                .collect();
            ensure!(
                values
                    .first()
                    .is_none_or(|first| values.iter().all(|value| value == first)),
                "several incoming versions conflict in {path}; keep local, edit the file, and commit to resolve all alternatives"
            );
            values.first().cloned().cloned().flatten()
        };
        if let Some(value) = value {
            files.insert(path.clone(), value);
        }
    }
    let snapshot = Snapshot { files };
    snapshot.validate()?;
    Ok(snapshot)
}
