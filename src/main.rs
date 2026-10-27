use toshokan::api::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let mut terminal = ratatui::init();
    let result = toshokan::app::run(&mut terminal, client).await;
    ratatui::restore();
    Ok(result?)
}
