use structopt::StructOpt;

use crate::Result;

#[derive(Debug, StructOpt)]
pub enum Command {
  Token,
  Connect,
  StartTestGame,
}

impl Command {
  #[tracing::instrument(skip(self))]
  pub async fn run(&self, player_id: i32) -> Result<()> {
    let token = flo_controller::player::token::create_player_token(player_id)?;
    match *self {
      Command::Token => println!("{}", token),
      Command::Connect => {
        let token = flo_controller::player::token::create_player_token(player_id)?;
        tracing::debug!("token generated: {}", token);
        let client = flo_client::start(flo_client::StartConfig {
          token: Some(token),
          controller_host: "127.0.0.1".to_string().into(),
          ..Default::default()
        })
        .await
        .unwrap();
        client.serve().await;
      }
      Command::StartTestGame => {
        let client = flo_client::start(Default::default()).await.unwrap();
        client.start_test_game().await.unwrap();
        client.serve().await;
      }
    }

    Ok(())
  }
}
