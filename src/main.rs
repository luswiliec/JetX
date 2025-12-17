use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use csv::Writer;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use chrono::{DateTime, Utc};
use std::error::Error;
use std::fs::OpenOptions;
use std::collections::HashMap;
use std::env;
use actix_web::{get, App, HttpResponse, HttpServer, Responder};

#[derive(Debug, Serialize, Deserialize)]
struct GameRound {
    date: String,
    time: String,
    crash_multiplier: f64,
    flight_duration: f64,
    total_bets_usd: f64,
    total_players_bet: usize,
    total_cashouts_usd: f64,
    total_players_cashed_out: usize,
    profit_usd: f64,
    players_lost: usize,
}

#[derive(Debug, Clone)]
struct PlayerBet {
    username: String,
    player_id: String,
    bet_amount_usd: f64,
    currency: String,
    bet_number: String,
}

#[derive(Debug, Clone)]
struct PlayerCashout {
    username: String,
    player_id: String,
    bet_amount_usd: f64,
    multiplier: f64,
    cashout_amount_usd: f64,
}

#[derive(Debug)]
struct RoundTracker {
    bets: HashMap<String, PlayerBet>,
    cashouts: Vec<PlayerCashout>,
    start_time: Option<DateTime<Utc>>,
    crash_multiplier: f64,
    flight_duration: f64,
    is_active: bool,
}

impl RoundTracker {
    fn new() -> Self {
        Self {
            bets: HashMap::new(),
            cashouts: Vec::new(),
            start_time: None,
            crash_multiplier: 0.0,
            flight_duration: 0.0,
            is_active: false,
        }
    }

    fn reset(&mut self) {
        self.bets.clear();
        self.cashouts.clear();
        self.start_time = None;
        self.crash_multiplier = 0.0;
        self.flight_duration = 0.0;
        self.is_active = false;
    }

    fn calculate_stats(&self) -> GameRound {
        let total_bets_usd: f64 = self.bets.values().map(|b| b.bet_amount_usd).sum();
        let total_cashouts_usd: f64 = self.cashouts.iter().map(|c| c.cashout_amount_usd).sum();
        let total_players_bet = self.bets.len();
        let total_players_cashed_out = self.cashouts.len();
        // Safely calculate players lost - handle case where cashouts might exceed bets
        let players_lost = if total_players_cashed_out > total_players_bet {
            0
        } else {
            total_players_bet - total_players_cashed_out
        };
        let profit_usd = total_bets_usd - total_cashouts_usd;

        let start_time = self.start_time.unwrap_or_else(Utc::now);
        
        GameRound {
            date: start_time.format("%Y-%m-%d").to_string(),
            time: start_time.format("%H:%M:%S").to_string(),
            crash_multiplier: (self.crash_multiplier * 100.0).round() / 100.0,
            flight_duration: (self.flight_duration * 100.0).round() / 100.0,
            total_bets_usd: (total_bets_usd * 100.0).round() / 100.0,
            total_players_bet,
            total_cashouts_usd: (total_cashouts_usd * 100.0).round() / 100.0,
            total_players_cashed_out,
            profit_usd: (profit_usd * 100.0).round() / 100.0,
            players_lost,
        }
    }
}

fn parse_player_data(data_str: &str) -> Option<Vec<String>> {
    // Split by underscore: username_bet_local_mult_cashout_id_betnum_currency_unknown
    let parts: Vec<&str> = data_str.split('_').collect();
    if parts.len() >= 9 {
        Some(parts.iter().map(|s| s.to_string()).collect())
    } else {
        None
    }
}

// HTTP endpoints for keeping the service alive
#[get("/")]
async fn hello() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "service": "JetX Game Data Monitor",
        "uptime": "running"
    }))
}

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "healthy",
        "timestamp": Utc::now().to_rfc3339()
    }))
}

#[get("/status")]
async fn status() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "monitoring",
        "service": "JetX WebSocket Monitor",
        "timestamp": Utc::now().to_rfc3339()
    }))
}

// WebSocket monitoring function
async fn run_websocket_monitor() {
    loop {
        match monitor_jetx().await {
            Ok(_) => {
                println!("⚠️  WebSocket connection ended normally. Reconnecting in 5 seconds...");
            }
            Err(e) => {
                eprintln!("❌ WebSocket error: {}. Reconnecting in 5 seconds...", e);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

async fn monitor_jetx() -> Result<(), Box<dyn Error>> {
    let ws_url = "wss://eu-server-w4.ssgportal.com/JetXNode703/signalr/connect?transport=webSockets&clientProtocol=1.5&token=2c31ab56-d885-46a5-bdf2-c9249136a39c&group=JetX&connectionToken=4%2BMbXiGbp3b9sUw36wIpaGI%2BboWqyfz8EvXsRYDuxEPGkxOsN2y22pdFSjUdBWxVmzQxhpcyF5ZXvjj6vhy1jtx4QYvtuIWPe52aU4RZ%2FD6r79v7%2FnHSjRSmnZPvLPHj&connectionData=%5B%7B%22name%22%3A%22h%22%7D%5D&tid=2";

    println!("🔌 Connecting to WebSocket: {}", ws_url);

    let (ws_stream, _) = connect_async(ws_url).await?;
    println!("✅ WebSocket connection established");

    let (mut write, mut read) = ws_stream.split();

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .append(true)
        .open("jetx_game_data.csv")?;

    let mut csv_writer = Writer::from_writer(file);

    // Check if file is empty to write header
    let metadata = std::fs::metadata("jetx_game_data.csv")?;
    if metadata.len() == 0 {
        csv_writer.write_record(&[
            "Date",
            "Time",
            "Crash Multiplier",
            "Flight Duration (s)",
            "Total Bets (USD)",
            "Total Players Bet",
            "Total Cashouts (USD)",
            "Total Players Cashed Out",
            "Profit (USD)",
            "Players Lost (Bet but No Cashout)",
        ])?;
        csv_writer.flush()?;
    }

    println!("📊 Listening for JetX game data...");
    println!("⚠️  NOTE: First round will be skipped (in-progress round)");
    println!("{}", "=".repeat(80));

    let mut round_tracker = RoundTracker::new();
    let mut round_count = 0;
    let mut first_round_seen = false;
    let mut message_counter = 0;

    while let Some(message) = read.next().await {
        match message {
            Ok(msg) => {
                match msg {
                    Message::Text(text) => {
                        message_counter += 1;
                        
                        println!("\n[MSG #{}] Received at {}", message_counter, Utc::now().format("%H:%M:%S%.3f"));
                        
                        if let Ok(json) = serde_json::from_str::<Value>(&text) {
                            if let Some(messages) = json["M"].as_array() {
                                for (idx, msg_obj) in messages.iter().enumerate() {
                                    println!("  [Sub-message {}]", idx + 1);
                                    
                                    if let Some(method) = msg_obj["M"].as_str() {
                                        if method == "response" {
                                            if let Some(args) = msg_obj["A"].as_array() {
                                                if let Some(arg) = args.first() {
                                                    let f = arg["f"].as_bool().unwrap_or(false);
                                                    let v = arg["v"].as_f64().unwrap_or(0.0);
                                                    let s = arg["s"].as_f64().unwrap_or(0.0);

                                                    println!("    Response: f={}, v={}, s={}", f, v, s);

                                                    if !f && v == 1.0 && s == 0.0 && !round_tracker.is_active {
                                                        round_tracker.start_time = Some(Utc::now());
                                                        round_tracker.is_active = true;
                                                        round_count += 1;
                                                        println!("\n🚀 [ROUND {}] FLIGHT STARTED at {}", 
                                                            round_count, 
                                                            Utc::now().format("%H:%M:%S"));
                                                        println!("{}", "-".repeat(80));
                                                    } else if !f && round_tracker.is_active {
                                                        round_tracker.crash_multiplier = v;
                                                        round_tracker.flight_duration = s;
                                                        print!("\r📈 Multiplier: {:.2}x | Time: {:.2}s", v, s);
                                                        std::io::Write::flush(&mut std::io::stdout()).ok();
                                                    } else if f && round_tracker.is_active {
                                                        round_tracker.crash_multiplier = v;
                                                        round_tracker.flight_duration = s;
                                                        println!("\n\n💥 CRASHED at {:.2}x (Duration: {:.2}s)", v, s);
                                                        println!("{}", "-".repeat(80));

                                                        let round_stats = round_tracker.calculate_stats();
                                                        
                                                        println!("\n📊 ROUND {} SUMMARY:", round_count);
                                                        println!("   Total Bets: ${:.2} from {} players", 
                                                            round_stats.total_bets_usd, 
                                                            round_stats.total_players_bet);
                                                        println!("   Total Cashouts: ${:.2} from {} players", 
                                                            round_stats.total_cashouts_usd, 
                                                            round_stats.total_players_cashed_out);
                                                        println!("   Profit (House): ${:.2}", round_stats.profit_usd);
                                                        println!("   Players Lost: {}", round_stats.players_lost);

                                                        if first_round_seen {
                                                            println!("   ✅ Saved to CSV");
                                                            csv_writer.serialize(&round_stats)?;
                                                            csv_writer.flush()?;
                                                        } else {
                                                            println!("   ⚠️  SKIPPED (First incomplete round)");
                                                            first_round_seen = true;
                                                        }
                                                        
                                                        println!("{}", "=".repeat(80));

                                                        round_tracker.reset();
                                                    }
                                                }
                                            }
                                        } else if method == "g" {
                                            if let Some(args) = msg_obj["A"].as_array() {
                                                if let Some(arg) = args.first() {
                                                    if let Some(action_type) = arg["M"].as_str() {
                                                        println!("    Action type: {}", action_type);
                                                        
                                                        if let Some(info) = arg["I"].as_object() {
                                                            if let Some(data) = info.get("a").and_then(|v| v.as_str()) {
                                                                println!("    Data: {}", data);
                                                                
                                                                if let Some(parts) = parse_player_data(data) {
                                                                    if action_type == "b" && parts.len() >= 9 {
                                                                        let mult: f64 = parts[3].parse().unwrap_or(0.0);
                                                                        let cashout: f64 = parts[4].parse().unwrap_or(0.0);
                                                                        
                                                                        if mult == 0.0 && cashout == 0.0 {
                                                                            let bet = PlayerBet {
                                                                                username: parts[0].clone(),
                                                                                player_id: parts[5].clone(),
                                                                                bet_amount_usd: parts[1].parse().unwrap_or(0.0),
                                                                                currency: parts[7].clone(),
                                                                                bet_number: parts[6].clone(),
                                                                            };
                                                                            
                                                                            let key = format!("{}_{}", bet.player_id, bet.bet_number);
                                                                            println!("\n💰 BET: {} (ID: {}) placed ${:.2} {} [Bet #{}]",
                                                                                bet.username,
                                                                                bet.player_id,
                                                                                bet.bet_amount_usd,
                                                                                bet.currency,
                                                                                bet.bet_number);
                                                                            
                                                                            round_tracker.bets.insert(key, bet);
                                                                        } else {
                                                                            println!("    ⚠️  Invalid bet (mult or cashout not 0)");
                                                                        }
                                                                    } else if action_type == "c" && parts.len() >= 9 {
                                                                        let mult: f64 = parts[3].parse().unwrap_or(0.0);
                                                                        let cashout_amt: f64 = parts[4].parse().unwrap_or(0.0);
                                                                        
                                                                        if mult > 0.0 && cashout_amt > 0.0 {
                                                                            let cashout = PlayerCashout {
                                                                                username: parts[0].clone(),
                                                                                player_id: parts[5].clone(),
                                                                                bet_amount_usd: parts[1].parse().unwrap_or(0.0),
                                                                                multiplier: mult,
                                                                                cashout_amount_usd: cashout_amt,
                                                                            };
                                                                            
                                                                            println!("\n✅ CASHOUT: {} (ID: {}) | Bet: ${:.2} | @{:.2}x | Won: ${:.2}",
                                                                                cashout.username,
                                                                                cashout.player_id,
                                                                                cashout.bet_amount_usd,
                                                                                cashout.multiplier,
                                                                                cashout.cashout_amount_usd);
                                                                            
                                                                            round_tracker.cashouts.push(cashout);
                                                                        } else {
                                                                            println!("    ⚠️  Invalid cashout (mult or cashout is 0)");
                                                                        }
                                                                    } else {
                                                                        println!("    ⚠️  Unrecognized action or incomplete data");
                                                                    }
                                                                } else {
                                                                    println!("    ⚠️  Failed to parse player data");
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        } else {
                                            println!("    Unknown method: {}", method);
                                        }
                                    } else {
                                        println!("    No method field found");
                                    }
                                }
                            } else {
                                println!("  No M array in message");
                            }
                        } else {
                            println!("  Failed to parse as JSON");
                        }
                    }

                    Message::Binary(data) => {
                        message_counter += 1;
                        println!("\n[MSG #{}] Binary data received: {} bytes", message_counter, data.len());
                    }

                    Message::Ping(data) => {
                        println!("\n[PING] Received, sending pong...");
                        write.send(Message::Pong(data)).await?;
                    }

                    Message::Pong(_) => {
                        println!("\n[PONG] Received");
                    }

                    Message::Close(frame) => {
                        println!("\n[CLOSE] Connection closed by server: {:?}", frame);
                        break;
                    }

                    Message::Frame(_) => {
                        message_counter += 1;
                        println!("\n[MSG #{}] Frame message", message_counter);
                    }
                }
            }

            Err(e) => {
                eprintln!("\n[ERROR] Error receiving message: {}", e);
                return Err(e.into());
            }
        }
    }

    println!("\n\n=== CONNECTION ENDED ===");
    println!("Total messages received: {}", message_counter);
    println!("Total rounds tracked: {}", round_count);
    
    Ok(())
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = env::var("PORT")
        .unwrap_or("8000".to_string())
        .parse()
        .unwrap();
    
    println!("🚀 Starting JetX Monitor Service");
    println!("🌐 Web server on port {}", port);
    println!("💓 Configure cron-job.org to ping: http://your-app.koyeb.app/health");
    println!("   Recommended: Every 5 minutes");
    println!("{}", "=".repeat(80));
    
    // Spawn WebSocket monitor as a background task
    tokio::spawn(async {
        run_websocket_monitor().await;
    });
    
    // Start HTTP server
    HttpServer::new(|| {
        App::new()
            .service(hello)
            .service(health)
            .service(status)
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
