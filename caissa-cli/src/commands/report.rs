pub async fn run(farga_url: &str, project: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/signals/recent?project={}&since=1h", farga_url, project);
    match client.get(&url).send().await {
        Ok(resp) => println!(
            "Farga at {} — status: {} (project: {})",
            farga_url,
            resp.status(),
            project
        ),
        Err(e) => println!("Farga at {} — unreachable: {}", farga_url, e),
    }
    Ok(())
}
