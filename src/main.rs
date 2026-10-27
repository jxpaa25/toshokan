use ratatui_image::picker::Picker;
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
    // Ask the terminal which image protocol it supports before anything else reads input.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let result = toshokan::app::run(&mut terminal, client, config, picker).await;
    ratatui::restore();
    Ok(result?)
}
