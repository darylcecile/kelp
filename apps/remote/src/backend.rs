use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use kelp_core::{
    object_id, storage,
    transactions::{JournalPage, SyncPage, Transaction, partition},
    transfer::{self, Info, Key, Object},
};
use rusqlite::{Connection, params};

use crate::Error;

#[derive(Clone)]
pub enum Backend {
    Local(Arc<Mutex<Connection>>),
    Cluster {
        nodes: Arc<Vec<String>>,
        token: Arc<str>,
        client: reqwest::Client,
    },
}

impl Backend {
    pub async fn info_batch(&self, project: &str, keys: Vec<Key>) -> Result<Vec<Info>, Error> {
        match self {
            Self::Local(_) => {
                let project = project.to_owned();
                self.local_run(move |db| {
                    require_project(db, &project)?;
                    keys.into_iter()
                        .map(|key| {
                            let size = storage::size(db, &project, &key.kind, &key.id)?;
                            Ok(Info { object: key, size })
                        })
                        .collect()
                })
                .await
            }
            Self::Cluster {
                nodes,
                token,
                client,
            } => {
                let mut calls = tokio::task::JoinSet::new();
                for node in nodes.iter() {
                    let request = client
                        .post(format!("{node}/storage/projects/{project}/objects/info"))
                        .bearer_auth(token.as_ref())
                        .json(&transfer::Request {
                            objects: keys.clone(),
                        });
                    calls.spawn(async move {
                        response(request)
                            .await?
                            .json::<Vec<Info>>()
                            .await
                            .map_err(unavailable)
                    });
                }
                let mut sizes = std::collections::BTreeMap::new();
                while let Some(reply) = calls.join_next().await {
                    for info in reply.map_err(|error| Error::Internal(error.into()))?? {
                        if let Some(size) = info.size {
                            sizes.insert(info.object, size);
                        }
                    }
                }
                Ok(keys
                    .into_iter()
                    .map(|key| Info {
                        size: sizes.get(&key).copied(),
                        object: key,
                    })
                    .collect())
            }
        }
    }

    pub async fn get_batch(&self, project: &str, keys: Vec<Key>) -> Result<Vec<Object>, Error> {
        if matches!(self, Self::Local(_)) {
            let project = project.to_owned();
            return self
                .local_run(move |db| {
                    require_project(db, &project)?;
                    keys.into_iter()
                        .map(|key| {
                            Ok(Object {
                                bytes: storage::get(db, &project, &key.kind, &key.id)?,
                                key,
                            })
                        })
                        .collect()
                })
                .await;
        }
        let mut objects = Vec::new();
        for group in keys.chunks(16) {
            let mut tasks = tokio::task::JoinSet::new();
            for key in group {
                let (backend, project, key) = (self.clone(), project.to_owned(), key.clone());
                tasks.spawn(async move {
                    Ok::<_, Error>(Object {
                        bytes: backend.get(&project, &key.kind, &key.id).await?,
                        key,
                    })
                });
            }
            while let Some(result) = tasks.join_next().await {
                objects.push(result.map_err(|error| Error::Internal(error.into()))??);
            }
        }
        Ok(objects)
    }

    pub async fn put_batch(&self, project: &str, objects: Vec<Object>) -> Result<(), Error> {
        match self {
            Self::Local(_) => {
                let project = project.to_owned();
                self.local_run(move |db| {
                    require_project(db, &project)?;
                    let transaction = db.transaction().map_err(anyhow::Error::from)?;
                    for object in objects {
                        storage::put(&transaction, &project, "blob", &object.bytes)?;
                    }
                    transaction.commit().map_err(anyhow::Error::from)?;
                    Ok(())
                })
                .await
            }
            Self::Cluster {
                nodes,
                token,
                client,
            } => {
                let mut groups: std::collections::BTreeMap<usize, Vec<Object>> = Default::default();
                for object in objects {
                    groups
                        .entry(partition(project, "blob", &object.key.id, nodes.len()))
                        .or_default()
                        .push(object);
                }
                let mut tasks = tokio::task::JoinSet::new();
                for (owner, objects) in groups {
                    let request = client
                        .post(format!(
                            "{}/storage/projects/{project}/objects/upload",
                            nodes[owner]
                        ))
                        .bearer_auth(token.as_ref())
                        .body(transfer::encode(&objects)?);
                    tasks.spawn(async move { response(request).await.map(|_| ()) });
                }
                while let Some(result) = tasks.join_next().await {
                    result.map_err(|error| Error::Internal(error.into()))??;
                }
                Ok(())
            }
        }
    }
    pub fn local(directory: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let db = storage::open(&directory.join("kelp.sqlite3"))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS tx_projects (name TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS tx_journal (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                project TEXT NOT NULL REFERENCES tx_projects(name),
                transaction_id TEXT NOT NULL,
                UNIQUE(project, transaction_id)
             );
             CREATE INDEX IF NOT EXISTS tx_journal_project_cursor ON tx_journal(project, sequence);",
        )?;
        Ok(Self::Local(Arc::new(Mutex::new(db))))
    }

    pub fn cluster(mut nodes: Vec<String>, token: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !nodes.is_empty() && !token.is_empty(),
            "storage nodes and a storage token are required"
        );
        for node in &mut nodes {
            let url = reqwest::Url::parse(node)?;
            anyhow::ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "invalid storage URL"
            );
            *node = url.as_str().trim_end_matches('/').to_owned();
        }
        nodes.sort();
        let original = nodes.len();
        nodes.dedup();
        anyhow::ensure!(
            original == nodes.len(),
            "storage nodes must have distinct URLs"
        );
        Ok(Self::Cluster {
            nodes: Arc::new(nodes),
            token: token.into(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    pub fn nodes(&self) -> usize {
        match self {
            Self::Local(_) => 1,
            Self::Cluster { nodes, .. } => nodes.len(),
        }
    }

    pub fn layout(&self) -> String {
        match self {
            Self::Local(_) => "local".into(),
            Self::Cluster { nodes, .. } => object_id("layout", nodes.join("\0").as_bytes()),
        }
    }

    async fn local_run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let Self::Local(db) = self else {
            return Err(Error::Internal(anyhow::anyhow!(
                "operation requires a local store"
            )));
        };
        let db = db.clone();
        tokio::task::spawn_blocking(move || {
            let mut db = db
                .lock()
                .map_err(|_| Error::Internal(anyhow::anyhow!("storage lock poisoned")))?;
            operation(&mut db)
        })
        .await
        .map_err(|error| Error::Internal(error.into()))?
    }

    pub async fn project(&self, project: &str, create: bool) -> Result<(), Error> {
        let name = project.to_owned();
        match self {
            Self::Local(_) => {
                self.local_run(move |db| {
                    if create {
                        db.execute(
                            "INSERT OR IGNORE INTO tx_projects(name) VALUES (?1)",
                            [&name],
                        )
                        .map_err(anyhow::Error::from)?;
                    }
                    require_project(db, &name)
                })
                .await
            }
            Self::Cluster {
                nodes,
                token,
                client,
            } => {
                let mut calls = tokio::task::JoinSet::new();
                for node in nodes.iter() {
                    let request = if create {
                        client.put(format!("{node}/storage/projects/{project}"))
                    } else {
                        client.get(format!("{node}/storage/projects/{project}"))
                    };
                    let token = token.clone();
                    let node = node.clone();
                    calls.spawn(async move {
                        (
                            node,
                            response(request.bearer_auth(token.as_ref()))
                                .await
                                .map(|_| ()),
                        )
                    });
                }
                let mut found = false;
                let mut missing = Vec::new();
                while let Some(result) = calls.join_next().await {
                    let (node, result) = result.map_err(|error| Error::Internal(error.into()))?;
                    match result {
                        Ok(()) => found = true,
                        Err(Error::Missing(_)) if !create => missing.push(node),
                        Err(error) => return Err(error),
                    }
                }
                if !found {
                    return Err(Error::Missing(format!("project {project} does not exist")));
                }
                for node in missing {
                    response(
                        client
                            .put(format!("{node}/storage/projects/{project}"))
                            .bearer_auth(token.as_ref()),
                    )
                    .await?;
                }
                Ok(())
            }
        }
    }

    pub async fn get(&self, project: &str, kind: &str, id: &str) -> Result<Vec<u8>, Error> {
        match self {
            Self::Local(_) => {
                let (project, kind, id) = (project.to_owned(), kind.to_owned(), id.to_owned());
                self.local_run(move |db| {
                    require_project(db, &project)?;
                    if !storage::contains(db, &project, &kind, &id)? {
                        return Err(Error::Missing(format!("missing {kind} {id}")));
                    }
                    Ok(storage::get(db, &project, &kind, &id)?)
                })
                .await
            }
            Self::Cluster {
                nodes,
                token,
                client,
            } => {
                let bytes = locate(nodes, client, token, project, kind, id, false)
                    .await?
                    .bytes()
                    .await
                    .map_err(unavailable)?
                    .to_vec();
                if object_id(kind, &bytes) != id {
                    return Err(Error::Unavailable("storage returned corrupt data".into()));
                }
                Ok(bytes)
            }
        }
    }

    pub async fn size(&self, project: &str, kind: &str, id: &str) -> Result<u64, Error> {
        match self {
            Self::Local(_) => {
                let (project, kind, id) = (project.to_owned(), kind.to_owned(), id.to_owned());
                self.local_run(move |db| {
                    require_project(db, &project)?;
                    storage::size(db, &project, &kind, &id)?
                        .ok_or_else(|| Error::Missing("object is not available".into()))
                })
                .await
            }
            Self::Cluster {
                nodes,
                token,
                client,
            } => {
                let reply = locate(nodes, client, token, project, kind, id, true).await?;
                reply
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|header| header.to_str().ok())
                    .and_then(|size| size.parse().ok())
                    .ok_or_else(|| Error::Unavailable("storage omitted object length".into()))
            }
        }
    }

    pub async fn put_blob(&self, project: &str, id: &str, bytes: Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Local(_) => {
                let project = project.to_owned();
                self.local_run(move |db| {
                    require_project(db, &project)?;
                    storage::put(db, &project, "blob", &bytes)?;
                    Ok(())
                })
                .await
            }
            Self::Cluster {
                nodes,
                token,
                client,
            } => {
                let node = &nodes[partition(project, "blob", id, nodes.len())];
                response(
                    client
                        .put(format!(
                            "{node}/storage/projects/{project}/objects/blob/{id}"
                        ))
                        .bearer_auth(token.as_ref())
                        .body(bytes),
                )
                .await?;
                Ok(())
            }
        }
    }

    /// The gateway verifies dependencies and blob closure before this operation.
    /// One durable row publishes the entire transaction on its owning shard.
    pub async fn append(&self, project: &str, transaction: Transaction) -> Result<String, Error> {
        let id = transaction.id()?;
        match self {
            Self::Local(_) => {
                let project = project.to_owned();
                self.local_run(move |db| {
                    require_project(db, &project)?;
                    let tx = db.transaction().map_err(anyhow::Error::from)?;
                    let id = storage::put_json(&tx, &project, "transaction", &transaction)?;
                    tx.execute(
                        "INSERT OR IGNORE INTO tx_journal(project, transaction_id) VALUES (?1, ?2)",
                        params![project, id],
                    )
                    .map_err(anyhow::Error::from)?;
                    tx.commit().map_err(anyhow::Error::from)?;
                    Ok(id)
                })
                .await
            }
            Self::Cluster {
                nodes,
                token,
                client,
            } => {
                let node = &nodes[partition(project, "transaction", &id, nodes.len())];
                let receipt: kelp_core::transactions::Receipt = response(
                    client
                        .post(format!("{node}/storage/projects/{project}/transactions"))
                        .bearer_auth(token.as_ref())
                        .json(&transaction),
                )
                .await?
                .json()
                .await
                .map_err(unavailable)?;
                if receipt.transaction != id {
                    return Err(Error::Unavailable("storage receipt mismatch".into()));
                }
                Ok(id)
            }
        }
    }

    pub async fn journal(&self, project: &str, after: i64) -> Result<JournalPage, Error> {
        if after < 0 {
            return Err(Error::Invalid("negative journal cursor".into()));
        }
        let project = project.to_owned();
        self.local_run(move |db| {
            require_project(db, &project)?;
            let mut statement = db.prepare("SELECT sequence, transaction_id FROM tx_journal WHERE project = ?1 AND sequence > ?2 ORDER BY sequence LIMIT 257").map_err(anyhow::Error::from)?;
            let mut rows = statement.query_map(params![project, after], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
                .map_err(anyhow::Error::from)?.collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::from)?;
            let more = rows.len() > 256;
            if more { rows.pop(); }
            let cursor = rows.last().map(|(sequence, _)| *sequence).unwrap_or(after);
            Ok(JournalPage { transactions: rows.into_iter().map(|(_, id)| id).collect(), cursor, more })
        }).await
    }

    pub async fn sync(&self, project: &str, cursors: Vec<i64>) -> Result<SyncPage, Error> {
        let cursors = if cursors.is_empty() {
            vec![0; self.nodes()]
        } else {
            cursors
        };
        if cursors.len() != self.nodes() || cursors.iter().any(|cursor| *cursor < 0) {
            return Err(Error::Invalid(
                "cursor does not match the storage layout".into(),
            ));
        }
        let mut result = SyncPage {
            transactions: Default::default(),
            cursors: cursors.clone(),
            more: false,
        };
        match self {
            Self::Local(_) => {
                let page = self.journal(project, cursors[0]).await?;
                result.transactions.extend(page.transactions);
                result.cursors[0] = page.cursor;
                result.more = page.more;
            }
            Self::Cluster {
                nodes,
                token,
                client,
            } => {
                let mut calls = tokio::task::JoinSet::new();
                for (index, node) in nodes.iter().enumerate() {
                    let request = client
                        .get(format!("{node}/storage/projects/{project}/journal"))
                        .query(&[("after", cursors[index])])
                        .bearer_auth(token.as_ref());
                    calls.spawn(async move {
                        let page: JournalPage =
                            response(request).await?.json().await.map_err(unavailable)?;
                        Ok::<_, Error>((index, page))
                    });
                }
                while let Some(reply) = calls.join_next().await {
                    let (index, page) = reply.map_err(|error| Error::Internal(error.into()))??;
                    result.transactions.extend(page.transactions);
                    result.cursors[index] = page.cursor;
                    result.more |= page.more;
                }
            }
        }
        Ok(result)
    }
}

fn require_project(db: &Connection, name: &str) -> Result<(), Error> {
    let exists: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tx_projects WHERE name = ?1)",
            [name],
            |row| row.get(0),
        )
        .map_err(anyhow::Error::from)?;
    if exists {
        Ok(())
    } else {
        Err(Error::Missing(format!("project {name} does not exist")))
    }
}

fn unavailable(error: impl std::fmt::Display) -> Error {
    Error::Unavailable(format!("storage request failed: {error}"))
}

async fn response(request: reqwest::RequestBuilder) -> Result<reqwest::Response, Error> {
    let reply = request.send().await.map_err(unavailable)?;
    if reply.status().is_success() {
        return Ok(reply);
    }
    match reply.status() {
        reqwest::StatusCode::NOT_FOUND => Err(Error::Missing(
            "storage object or project is missing".into(),
        )),
        status => Err(Error::Unavailable(format!("storage returned {status}"))),
    }
}

// Existing immutable data can remain on an earlier owner when capacity is added.
// A miss probes the other members; new writes use the current placement.
async fn locate(
    nodes: &[String],
    client: &reqwest::Client,
    token: &str,
    project: &str,
    kind: &str,
    id: &str,
    head: bool,
) -> Result<reqwest::Response, Error> {
    let preferred = partition(project, kind, id, nodes.len());
    let mut failure = None;
    for index in
        std::iter::once(preferred).chain((0..nodes.len()).filter(|index| *index != preferred))
    {
        let url = format!(
            "{}/storage/projects/{project}/objects/{kind}/{id}",
            nodes[index]
        );
        let request = if head {
            client.head(url)
        } else {
            client.get(url)
        };
        match response(request.bearer_auth(token)).await {
            Ok(reply) => return Ok(reply),
            Err(Error::Missing(_)) => {}
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or_else(|| Error::Missing(format!("missing {kind} {id}"))))
}
