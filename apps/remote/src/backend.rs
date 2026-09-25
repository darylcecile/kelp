use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use kelp_core::{
    object_id, storage,
    transactions::{JournalPage, Pin, SyncPage, SyncRequest, Transaction, partition},
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
        replicas: usize,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredObject {
    pub project: String,
    pub key: Key,
}

impl StoredObject {
    pub fn cursor(&self) -> String {
        format!("{}\0{}\0{}", self.project, self.key.kind, self.key.id)
    }
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
                            Ok(Info {
                                object: key,
                                size,
                                copies: usize::from(size.is_some()),
                            })
                        })
                        .collect()
                })
                .await
            }
            Self::Cluster {
                nodes,
                token,
                client,
                ..
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
                let mut failures = 0;
                while let Some(reply) = calls.join_next().await {
                    let reply = reply.map_err(|error| Error::Internal(error.into()))?;
                    let Ok(infos) = reply else {
                        failures += 1;
                        continue;
                    };
                    for info in infos {
                        if let Some(size) = info.size {
                            let entry = sizes.entry(info.object).or_insert((size, 0));
                            if entry.0 != size {
                                return Err(Error::Unavailable("replica size mismatch".into()));
                            }
                            entry.1 += 1;
                        }
                    }
                }
                if failures >= self.replicas() {
                    return Err(Error::Unavailable(
                        "not enough storage replicas are reachable".into(),
                    ));
                }
                Ok(keys
                    .into_iter()
                    .map(|key| Info {
                        size: sizes.get(&key).map(|entry| entry.0),
                        copies: sizes.get(&key).map_or(0, |entry| entry.1),
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
                    require_writable(db)?;
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
                ..
            } => {
                let mut copies = std::collections::BTreeMap::<String, usize>::new();
                for offset in 0..nodes.len() {
                    let mut groups: std::collections::BTreeMap<usize, Vec<Object>> =
                        Default::default();
                    for object in &objects {
                        if copies.get(&object.key.id).copied().unwrap_or(0) < self.replicas() {
                            let owner = (partition(project, "blob", &object.key.id, nodes.len())
                                + offset)
                                % nodes.len();
                            groups.entry(owner).or_default().push(object.clone());
                        }
                    }
                    if groups.is_empty() {
                        break;
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
                        tasks.spawn(async move { (objects, response(request).await.is_ok()) });
                    }
                    while let Some(result) = tasks.join_next().await {
                        let (objects, success) =
                            result.map_err(|error| Error::Internal(error.into()))?;
                        if success {
                            for object in objects {
                                *copies.entry(object.key.id).or_default() += 1;
                            }
                        }
                    }
                }
                if objects.iter().any(|object| {
                    copies.get(&object.key.id).copied().unwrap_or(0) < self.replicas()
                }) {
                    return Err(Error::Unavailable(
                        "could not durably replicate every object".into(),
                    ));
                }
                Ok(())
            }
        }
    }
    pub fn local(directory: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let db = storage::open(&directory.join("kelp.sqlite3"))?;
        let indexed: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='tx_paths')",
            [],
            |row| row.get(0),
        )?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS tx_projects (name TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS tx_journal (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                project TEXT NOT NULL REFERENCES tx_projects(name),
                transaction_id TEXT NOT NULL,
                UNIQUE(project, transaction_id)
             );
             CREATE INDEX IF NOT EXISTS tx_journal_project_cursor ON tx_journal(project, sequence);
             CREATE TABLE IF NOT EXISTS tx_settings (key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
        )?;
        let tx = db.unchecked_transaction()?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS tx_paths (project TEXT NOT NULL, path TEXT NOT NULL, transaction_id TEXT NOT NULL, PRIMARY KEY(project,path,transaction_id));")?;
        if !indexed {
            let rows = tx
                .prepare("SELECT project,transaction_id FROM tx_journal")?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (project, id) in rows {
                let transaction: Transaction =
                    storage::get_json(&tx, &project, "transaction", &id)?;
                index_paths(&tx, &project, &id, &transaction)?;
            }
        }
        tx.commit()?;
        Ok(Self::Local(Arc::new(Mutex::new(db))))
    }

    pub fn cluster(nodes: Vec<String>, token: String) -> anyhow::Result<Self> {
        Self::replicated_cluster(nodes, token, 1)
    }

    pub fn replicated_cluster(
        mut nodes: Vec<String>,
        token: String,
        replicas: usize,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            replicas > 0 && replicas <= nodes.len(),
            "replica count must be between 1 and the number of storage nodes"
        );
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
            replicas,
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

    pub fn replicas(&self) -> usize {
        match self {
            Self::Local(_) => 1,
            Self::Cluster { replicas, .. } => *replicas,
        }
    }

    pub async fn drain(&self) -> Result<(), Error> {
        self.local_run(|db| {
            db.execute(
                "INSERT OR REPLACE INTO tx_settings VALUES('draining',1)",
                [],
            )
            .map_err(anyhow::Error::from)?;
            Ok(())
        })
        .await
    }

    pub async fn inventory(&self, after: String) -> Result<Vec<StoredObject>, Error> {
        self.local_run(move |db| {
            Ok(db.prepare("SELECT n.name,k.name,lower(hex(o.hash)) FROM object_index o JOIN storage_namespaces n ON n.id=o.namespace JOIN storage_kinds k ON k.id=o.kind WHERE k.name IN ('blob','transaction','pin') AND n.name||char(0)||k.name||char(0)||lower(hex(o.hash))>?1 ORDER BY n.name,k.name,o.hash LIMIT 128")
                .map_err(anyhow::Error::from)?.query_map([after], |row| Ok(StoredObject { project: row.get(0)?, key: Key { kind: row.get(1)?, id: row.get(2)? } })).map_err(anyhow::Error::from)?.collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::from)?)
        }).await
    }

    pub async fn pins(&self, project: &str, after: &str) -> Result<Vec<Pin>, Error> {
        let mut pins = std::collections::BTreeMap::new();
        match self {
            Self::Local(_) => {
                let (project, after) = (project.to_owned(), after.to_owned());
                return self
                    .local_run(move |db| {
                        storage::ids(db, &project, "pin")?
                            .into_iter()
                            .filter(|id| id > &after)
                            .take(128)
                            .map(|id| Ok(storage::get_json(db, &project, "pin", &id)?))
                            .collect()
                    })
                    .await;
            }
            Self::Cluster {
                nodes,
                client,
                token,
                ..
            } => {
                let mut failures = 0;
                for node in nodes.iter() {
                    let reply = response(
                        client
                            .get(format!("{node}/storage/projects/{project}/pins"))
                            .query(&[("after", after)])
                            .bearer_auth(token.as_ref()),
                    )
                    .await;
                    let Ok(reply) = reply else {
                        failures += 1;
                        continue;
                    };
                    for pin in reply.json::<Vec<Pin>>().await.map_err(unavailable)? {
                        pins.insert(pin.id()?, pin);
                    }
                }
                if failures >= self.replicas() {
                    return Err(Error::Unavailable(
                        "not enough replicas to list release views".into(),
                    ));
                }
            }
        }
        Ok(pins.into_values().take(128).collect())
    }

    pub async fn put_pin(&self, project: &str, pin: Pin) -> Result<(), Error> {
        match self {
            Self::Local(_) => {
                let project = project.to_owned();
                self.local_run(move |db| {
                    require_writable(db)?;
                    require_project(db, &project)?;
                    storage::put_json(db, &project, "pin", &pin)?;
                    Ok(())
                })
                .await
            }
            Self::Cluster {
                nodes,
                client,
                token,
                ..
            } => {
                let owner = partition(project, "pin", &pin.id()?, nodes.len());
                let mut copies = 0;
                for offset in 0..nodes.len() {
                    let node = &nodes[(owner + offset) % nodes.len()];
                    if response(
                        client
                            .post(format!("{node}/storage/projects/{project}/pins"))
                            .bearer_auth(token.as_ref())
                            .json(&pin),
                    )
                    .await
                    .is_ok()
                    {
                        copies += 1;
                        if copies == self.replicas() {
                            return Ok(());
                        }
                    }
                }
                Err(Error::Unavailable(
                    "could not durably replicate release view".into(),
                ))
            }
        }
    }

    pub fn layout(&self) -> String {
        match self {
            Self::Local(_) => "local".into(),
            Self::Cluster {
                nodes, replicas, ..
            } => object_id(
                "layout",
                format!("{}\0{replicas}", nodes.join("\0")).as_bytes(),
            ),
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
                        require_writable(db)?;
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
                ..
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
                let mut failures = 0;
                let mut missing = Vec::new();
                while let Some(result) = calls.join_next().await {
                    let (node, result) = result.map_err(|error| Error::Internal(error.into()))?;
                    match result {
                        Ok(()) => found = true,
                        Err(Error::Missing(_)) if !create => missing.push(node),
                        Err(_) => failures += 1,
                    }
                }
                if failures >= self.replicas() {
                    return Err(Error::Unavailable(
                        "not enough project replicas are reachable".into(),
                    ));
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
                ..
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
                ..
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
        self.put_batch(
            project,
            vec![Object {
                key: Key {
                    kind: "blob".into(),
                    id: id.into(),
                },
                bytes,
            }],
        )
        .await
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
                    require_writable(&tx)?;
                    let id = storage::put_json(&tx, &project, "transaction", &transaction)?;
                    index_paths(&tx, &project, &id, &transaction)?;
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
                ..
            } => {
                let owner = partition(project, "transaction", &id, nodes.len());
                let mut copies = 0;
                for offset in 0..nodes.len() {
                    let node = &nodes[(owner + offset) % nodes.len()];
                    let reply = response(
                        client
                            .post(format!("{node}/storage/projects/{project}/transactions"))
                            .bearer_auth(token.as_ref())
                            .json(&transaction),
                    )
                    .await;
                    let Ok(reply) = reply else {
                        continue;
                    };
                    let receipt: kelp_core::transactions::Receipt =
                        reply.json().await.map_err(unavailable)?;
                    if receipt.transaction != id {
                        return Err(Error::Unavailable("storage receipt mismatch".into()));
                    }
                    copies += 1;
                    if copies == self.replicas() {
                        return Ok(id);
                    }
                }
                Err(Error::Unavailable(
                    "could not durably replicate the transaction journal".into(),
                ))
            }
        }
    }

    pub async fn journal(&self, project: &str, after: i64) -> Result<JournalPage, Error> {
        self.journal_selected(project, after, Vec::new()).await
    }

    async fn journal_selected(
        &self,
        project: &str,
        after: i64,
        paths: Vec<String>,
    ) -> Result<JournalPage, Error> {
        if after < 0 {
            return Err(Error::Invalid("negative journal cursor".into()));
        }
        let project = project.to_owned();
        self.local_run(move |db| {
            require_project(db, &project)?;
            let mut sql = "SELECT sequence, transaction_id FROM tx_journal j WHERE project = ?1 AND sequence > ?2".to_owned();
            let mut args: Vec<rusqlite::types::Value> = vec![project.into(), after.into()];
            if !paths.is_empty() {
                sql.push_str(" AND EXISTS(SELECT 1 FROM tx_paths p WHERE p.project=j.project AND p.transaction_id=j.transaction_id AND (");
                let predicates: Vec<_> = paths.iter().enumerate().map(|(index, path)| {
                    args.push(path.clone().into());
                    let n = index + 3;
                    format!("(p.path=?{n} OR (p.path>=?{n}||'/' AND p.path<?{n}||'0') OR (?{n}>=p.path||'/' AND ?{n}<p.path||'0'))")
                }).collect();
                sql.push_str(&predicates.join(" OR "));
                sql.push_str("))");
            }
            sql.push_str(" ORDER BY sequence LIMIT 257");
            let mut statement = db.prepare(&sql).map_err(anyhow::Error::from)?;
            let mut rows = statement.query_map(rusqlite::params_from_iter(args), |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
                .map_err(anyhow::Error::from)?.collect::<Result<Vec<_>, _>>().map_err(anyhow::Error::from)?;
            let more = rows.len() > 256;
            if more { rows.pop(); }
            let cursor = rows.last().map(|(sequence, _)| *sequence).unwrap_or(after);
            Ok(JournalPage { transactions: rows.into_iter().map(|(_, id)| id).collect(), cursor, more })
        }).await
    }

    pub async fn sync_selected(
        &self,
        project: &str,
        cursors: Vec<i64>,
        paths: Vec<String>,
    ) -> Result<SyncPage, Error> {
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
                let page = self.journal_selected(project, cursors[0], paths).await?;
                result.transactions.extend(page.transactions);
                result.cursors[0] = page.cursor;
                result.more = page.more;
            }
            Self::Cluster {
                nodes,
                token,
                client,
                ..
            } => {
                let mut calls = tokio::task::JoinSet::new();
                for (index, node) in nodes.iter().enumerate() {
                    let request = client
                        .post(format!("{node}/storage/projects/{project}/sync"))
                        .json(&SyncRequest {
                            cursors: vec![cursors[index]],
                            paths: paths.clone(),
                        })
                        .bearer_auth(token.as_ref());
                    calls.spawn(async move {
                        let page: SyncPage =
                            response(request).await?.json().await.map_err(unavailable)?;
                        Ok::<_, Error>((index, page))
                    });
                }
                let mut failures = 0;
                while let Some(reply) = calls.join_next().await {
                    let reply = reply.map_err(|error| Error::Internal(error.into()))?;
                    let Ok((index, page)) = reply else {
                        failures += 1;
                        continue;
                    };
                    result.transactions.extend(page.transactions);
                    result.cursors[index] = *page
                        .cursors
                        .first()
                        .ok_or_else(|| Error::Unavailable("missing storage cursor".into()))?;
                    result.more |= page.more;
                }
                if failures >= self.replicas() {
                    return Err(Error::Unavailable(
                        "not enough journal replicas are reachable for a complete sync".into(),
                    ));
                }
            }
        }
        Ok(result)
    }
}

fn index_paths(
    db: &Connection,
    project: &str,
    id: &str,
    transaction: &Transaction,
) -> anyhow::Result<()> {
    let mut insert = db.prepare_cached(
        "INSERT OR IGNORE INTO tx_paths(project,path,transaction_id) VALUES(?1,?2,?3)",
    )?;
    for path in transaction.edits.keys() {
        insert.execute(params![project, path, id])?;
    }
    Ok(())
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

fn require_writable(db: &Connection) -> Result<(), Error> {
    let draining: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tx_settings WHERE key='draining' AND value=1)",
            [],
            |row| row.get(0),
        )
        .map_err(anyhow::Error::from)?;
    if draining {
        return Err(Error::Unavailable("storage node is draining".into()));
    }
    Ok(())
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
