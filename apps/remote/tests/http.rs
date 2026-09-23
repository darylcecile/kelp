use kelp_core::ProjectInfo;

#[tokio::test]
async fn http_requires_authentication_and_persists_projects() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = kelp_remote::app(directory.path(), "test-token".into())?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::new();
    assert_eq!(
        client.get(format!("{url}/healthz")).send().await?.status(),
        200
    );
    assert_eq!(
        client
            .put(format!("{url}/v0/projects/demo"))
            .send()
            .await?
            .status(),
        401
    );
    let response = client
        .put(format!("{url}/v0/projects/demo"))
        .bearer_auth("test-token")
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    let info: ProjectInfo = response.json().await?;
    assert_eq!(info.project, "demo");
    assert_eq!(
        client
            .get(format!("{url}/v0/projects/demo"))
            .bearer_auth("wrong-token")
            .send()
            .await?
            .status(),
        401
    );
    server.abort();
    Ok(())
}
