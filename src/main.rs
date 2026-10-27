use toshokan::api::Client;
use toshokan::config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = match Config::load(Config::default_path()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("toshokan: invalid config file {err}");
            std::process::exit(1);
        }
    };
    let client = Client::new()?;
    let mut terminal = ratatui::init();
    let result = toshokan::app::run(&mut terminal, client, config).await;
    ratatui::restore();
    Ok(result?)
}
