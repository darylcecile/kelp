use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use kelp_core::transactions::{Pin, Transaction};

use crate::backend::{Backend, StoredObject};

#[derive(serde::Serialize)]
pub struct Report {
    pub objects: usize,
    pub nodes: Vec<String>,
    pub replicas: usize,
    pub evacuated: Option<String>,
}

/// Re-establish replica placement, optionally draining one node before copying
/// its inventory. The source remains readable throughout and retains its data.
pub async fn maintain(
    mut nodes: Vec<String>,
    token: String,
    replicas: usize,
    evacuate: Option<String>,
) -> Result<Report> {
    for node in &mut nodes {
        *node = node.trim_end_matches('/').into();
    }
    let evacuate = evacuate.map(|node| node.trim_end_matches('/').to_owned());
    let sources = if let Some(source) = &evacuate {
        ensure!(
            nodes.contains(source),
            "the retiring node must be in --shards"
        );
        nodes.retain(|node| node != source);
        vec![source.clone()]
    } else {
        nodes.clone()
    };
    let target = Backend::replicated_cluster(nodes.clone(), token.clone(), replicas)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    if let Some(source) = &evacuate {
        client
            .post(format!("{source}/storage/drain"))
            .bearer_auth(&token)
            .send()
            .await?
            .error_for_status()?;
    }
    let mut projects = BTreeSet::new();
    let mut objects = 0;
    for source in sources {
        let reader = Backend::cluster(vec![source.clone()], token.clone())?;
        let mut after = String::new();
        loop {
            let page: Vec<StoredObject> = client
                .get(format!("{source}/storage/inventory"))
                .query(&[("after", &after)])
                .bearer_auth(&token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if page.is_empty() {
                break;
            }
            for object in page {
                if projects.insert(object.project.clone()) {
                    target.project(&object.project, true).await?;
                }
                let bytes = reader
                    .get(&object.project, &object.key.kind, &object.key.id)
                    .await?;
                match object.key.kind.as_str() {
                    "blob" => {
                        target
                            .put_blob(&object.project, &object.key.id, bytes)
                            .await?
                    }
                    "transaction" => {
                        let transaction: Transaction = serde_json::from_slice(&bytes)?;
                        ensure!(
                            target.append(&object.project, transaction).await? == object.key.id,
                            "transaction changed during evacuation"
                        );
                    }
                    "pin" => {
                        let pin: Pin = serde_json::from_slice(&bytes)?;
                        ensure!(pin.id()? == object.key.id, "release view hash mismatch");
                        target.put_pin(&object.project, pin).await?;
                    }
                    _ => anyhow::bail!("unsupported stored object kind"),
                }
                after = object.cursor();
                objects += 1;
            }
        }
    }
    Ok(Report {
        objects,
        nodes,
        replicas,
        evacuated: evacuate,
    })
}
