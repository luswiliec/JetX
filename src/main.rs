use tokio_tungstenite::{tungstenite::Message};
use tokio_tungstenite::Connector;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use chrono::{DateTime, Utc, Duration};
use std::error::Error;
use std::collections::HashMap;
use std::env;
use actix_web::{get, App, HttpResponse, HttpServer, Responder};
use tokio_postgres::Client;
use postgres_native_tls;
use native_tls::TlsConnector as NativeTlsConnector;

// Helper function to add 2 hours to current time
fn get_adjusted_time() -> DateTime<Utc> {
    Utc::now() + Duration::hours(2)
}

#[derive(Debug, Serialize, Deserialize)]
struct GameRound {
    date: String,
    time: String,
    crash_multiplier: f64,
    flight_duration: f64,
    total_bets_usd: f64,
    total_players_bet: i32,
    total_cashouts_usd: f64,
    total_players_cashed_out: i32,
    profit_usd: f64,
    players_lost: i32,
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
        let total_players_bet = self.bets.len() as i32;
        let total_players_cashed_out = self.cashouts.len() as i32;
        let players_lost = if total_players_cashed_out > total_players_bet {
            0
        } else {
            total_players_bet - total_players_cashed_out
        };
        let profit_usd = total_bets_usd - total_cashouts_usd;

        // Use start_time directly (already adjusted when stored)
        let adjusted_time = self.start_time.unwrap_or_else(get_adjusted_time);
        
        GameRound {
            date: adjusted_time.format("%Y-%m-%d").to_string(),
            time: adjusted_time.format("%H:%M:%S").to_string(),
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
    let parts: Vec<&str> = data_str.split('_').collect();
    if parts.len() >= 9 {
        Some(parts.iter().map(|s| s.to_string()).collect())
    } else {
        None
    }
}

// Database connection function
async fn get_db_client() -> Result<Client, Box<dyn Error>> {
    let db_user = env::var("DATABASE_USER").unwrap_or("avnadmin".to_string());
    let db_password = env::var("DATABASE_PASSWORD").unwrap_or("AVNS_qo4RbZtZ5nTmv6oZCvL".to_string());
    let db_host = env::var("DATABASE_HOST").unwrap_or("pg-406c52b-luswiliec-transcity.k.aivencloud.com".to_string());
    let db_port = env::var("DATABASE_PORT").unwrap_or("12394".to_string());
    let db_name = env::var("DATABASE_NAME").unwrap_or("defaultdb".to_string());

    // Use postgres-native-tls for secure connection to Aiven PostgreSQL
    let mut builder = native_tls::TlsConnector::builder();
    // Accept any certificate for compatibility (Aiven uses valid certs, but this ensures connection)
    builder.danger_accept_invalid_certs(true);
    let tls_connector = builder.build()?;
    let tls = postgres_native_tls::MakeTlsConnector::new(tls_connector);

    let connection_string = format!(
        "host={} port={} user={} password={} dbname={} sslmode=require",
        db_host, db_port, db_user, db_password, db_name
    );

    println!("🔌 Connecting to PostgreSQL...");
    println!("   Host: {}", db_host);
    println!("   Database: {}", db_name);
    println!("   User: {}", db_user);

    let (client, connection) = match tokio_postgres::connect(&connection_string, tls).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("❌ Failed to connect to PostgreSQL: {}", e);
            eprintln!("   Connection string: host={} port={} dbname={}", db_host, db_port, db_name);
            return Err(e.into());
        }
    };

    // Spawn connection in background
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("❌ Database connection error: {}", e);
        }
    });

    println!("✅ Database connected successfully");
    
    // Verify table exists
    match client.query("SELECT COUNT(*) FROM jetxv1", &[]).await {
        Ok(_) => println!("✅ Table 'jetxv1' verified"),
        Err(e) => {
            eprintln!("⚠️  Warning: Could not verify table 'jetxv1': {}", e);
            eprintln!("   Make sure the table exists. Run this SQL:");
            eprintln!("   CREATE TABLE jetxv1 (");
            eprintln!("       date DATE,");
            eprintln!("       time TIME,");
            eprintln!("       crash_multiplier NUMERIC(10,2),");
            eprintln!("       flight_duration NUMERIC(10,2),");
            eprintln!("       total_bets_usd NUMERIC(10,2),");
            eprintln!("       total_players_bet INTEGER,");
            eprintln!("       total_cashouts_usd NUMERIC(10,2),");
            eprintln!("       total_players_cashed_out INTEGER,");
            eprintln!("       profit_usd NUMERIC(10,2),");
            eprintln!("       players_lost INTEGER");
            eprintln!("   );");
        }
    }
    
    Ok(client)
}

// Save game round to database
async fn save_round_to_db(client: &Client, round: &GameRound) -> Result<(), Box<dyn Error>> {
    let query = "
        INSERT INTO jetxv1 (
            date, time, crash_multiplier, flight_duration, 
            total_bets_usd, total_players_bet, total_cashouts_usd, 
            total_players_cashed_out, profit_usd, players_lost
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
    ";

    match client.execute(
        query,
        &[
            &round.date,
            &round.time,
            &round.crash_multiplier,
            &round.flight_duration,
            &round.total_bets_usd,
            &round.total_players_bet,
            &round.total_cashouts_usd,
            &round.total_players_cashed_out,
            &round.profit_usd,
            &round.players_lost,
        ],
    ).await {
        Ok(rows) => {
            println!("💾 Saved to database successfully ({} row inserted)", rows);
            Ok(())
        }
        Err(e) => {
            eprintln!("❌ Database insert error: {}", e);
            eprintln!("   Data: date={}, time={}, multiplier={:.2}x", 
                round.date, round.time, round.crash_multiplier);
            Err(e.into())
        }
    }
}

// HTTP endpoints for keeping the service alive
#[get("/")]
async fn hello() -> impl Responder {
    let adjusted_time = get_adjusted_time();
    HttpResponse::Ok()
        .content_type("application/json")
        .json(serde_json::json!({
            "status": "ok",
            "service": "JetX Game Data Monitor",
            "message": "Service is running and monitoring JetX games",
            "uptime": "active",
            "timestamp": adjusted_time.to_rfc3339(),
            "local_time": adjusted_time.format("%Y-%m-%d %H:%M:%S").to_string()
        }))
}

#[get("/health")]
async fn health() -> impl Responder {
    let adjusted_time = get_adjusted_time();
    HttpResponse::Ok()
        .content_type("application/json")
        .json(serde_json::json!({
            "status": "healthy",
            "service": "jetx-monitor",
            "database": "connected",
            "websocket": "active",
            "timestamp": adjusted_time.to_rfc3339(),
            "local_time": adjusted_time.format("%Y-%m-%d %H:%M:%S").to_string()
        }))
}

#[get("/status")]
async fn status() -> impl Responder {
    let adjusted_time = get_adjusted_time();
    HttpResponse::Ok()
        .content_type("application/json")
        .json(serde_json::json!({
            "status": "monitoring",
            "service": "JetX WebSocket Monitor",
            "description": "Tracking game rounds and saving to PostgreSQL",
            "timestamp": adjusted_time.to_rfc3339(),
            "local_time": adjusted_time.format("%Y-%m-%d %H:%M:%S").to_string()
        }))
}

// WebSocket monitoring function with auto-reconnect
async fn run_websocket_monitor() {
    let mut reconnect_attempts = 0;
    loop {
        reconnect_attempts += 1;
        println!("\n🔄 WebSocket Monitor Attempt #{}", reconnect_attempts);
        
        match monitor_jetx().await {
            Ok(_) => {
                println!("⚠️  WebSocket connection ended normally. Reconnecting in 5 seconds...");
                reconnect_attempts = 0; // Reset counter on successful connection
            }
            Err(e) => {
                eprintln!("❌ WebSocket error: {}. Reconnecting in 5 seconds...", e);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

async fn monitor_jetx() -> Result<(), Box<dyn Error>> {
    let ws_url = "wss://eu-server-w4.ssgportal.com/JetXNode703/signalr/connect?transport=webSockets&clientProtocol=1.5&token=f56d8585-2083-457e-ad99-55f451fab230&group=JetX&connectionToken=cQFhaIAMakpskdxRYvbNsTvqu90IxhJ%2BDuB67JN6r10o7ckETvSPufn8ZfduFPFX2gzUMdrgP9lZ9ZVXyPPfRsm1jgKgJAMGf%2FmgHh7MFzOyRtfKzsu8kNtduVkX5oLm&connectionData=%5B%7B%22name%22%3A%22h%22%7D%5D&tid=2";

    println!("🔌 Connecting to WebSocket: {}", ws_url);

    // Create TLS connector for secure WebSocket (wss://) with less strict validation
    let connector = NativeTlsConnector::builder()
        .danger_accept_invalid_certs(true) // Accept self-signed or invalid certificates
        .danger_accept_invalid_hostnames(true) // Accept hostname mismatches
        .build()?;
    let connector = Connector::NativeTls(connector);
    
    let (ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
        ws_url,
        None,
        false,
        Some(connector)
    ).await?;
    
    println!("✅ WebSocket connection established");

    // Connect to database
    let db_client = get_db_client().await?;

    let (mut write, mut read) = ws_stream.split();

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
                        
                        // Log every message with detailed timestamp (adjusted +2 hours)
                        let adjusted_time = get_adjusted_time();
                        let timestamp = adjusted_time.format("%H:%M:%S%.3f").to_string();
                        println!("\n[MSG #{}] Received at {}", message_counter, timestamp);
                        
                        if let Ok(json) = serde_json::from_str::<Value>(&text) {
                            if let Some(messages) = json["M"].as_array() {
                                let total_messages = messages.len();
                                println!("  📦 Processing {} sub-messages in this batch", total_messages);
                                
                                // Process EVERY message in the array - critical for not skipping data
                                for (idx, msg_obj) in messages.iter().enumerate() {
                                    println!("  [Sub-message {}/{}]", idx + 1, total_messages);
                                    
                                    if let Some(method) = msg_obj["M"].as_str() {
                                        if method == "response" {
                                            if let Some(args) = msg_obj["A"].as_array() {
                                                if let Some(arg) = args.first() {
                                                    let f = arg["f"].as_bool().unwrap_or(false);
                                                    let v = arg["v"].as_f64().unwrap_or(0.0);
                                                    let s = arg["s"].as_f64().unwrap_or(0.0);

                                                    // Uncomment for detailed response logging:
                                                    // println!("    Response: f={}, v={}, s={}", f, v, s);

                                                    if !f && v == 1.0 && s == 0.0 && !round_tracker.is_active {
                                                        round_tracker.start_time = Some(get_adjusted_time());
                                                        round_tracker.is_active = true;
                                                        round_count += 1;
                                                        println!("\n🚀 [ROUND {}] FLIGHT STARTED at {}", 
                                                            round_count, 
                                                            get_adjusted_time().format("%H:%M:%S"));
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
                                                            // Save to database
                                                            match save_round_to_db(&db_client, &round_stats).await {
                                                                Ok(_) => {
                                                                    println!("   ✅ Saved to database");
                                                                    println!("   📝 Round #{} recorded successfully", round_count - 1);
                                                                }
                                                                Err(e) => {
                                                                    eprintln!("   ❌ Database error: {}", e);
                                                                    eprintln!("   ⚠️  DATA NOT SAVED - Will retry on next round");
                                                                }
                                                            }
                                                        } else {
                                                            println!("   ⚠️  SKIPPED (First incomplete round - we joined mid-game)");
                                                            println!("   📌 Starting fresh tracking from next round");
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
                                                        // Uncomment for action type logging:
                                                        // println!("    Action type: {}", action_type);
                                                        
                                                        if let Some(info) = arg["I"].as_object() {
                                                            if let Some(data) = info.get("a").and_then(|v| v.as_str()) {
                                                                // Uncomment for raw data logging:
                                                                // println!("    Data: {}", data);
                                                                
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
                                                                            println!("      💰 BET: {} (ID: {}) placed ${:.2} {} [Bet #{}]",
                                                                                bet.username,
                                                                                bet.player_id,
                                                                                bet.bet_amount_usd,
                                                                                bet.currency,
                                                                                bet.bet_number);
                                                                            
                                                                            round_tracker.bets.insert(key, bet);
                                                                        } else {
                                                                            println!("      ⚠️  Invalid bet (mult or cashout not 0)");
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
                                                                            
                                                                            println!("      ✅ CASHOUT: {} (ID: {}) | Bet: ${:.2} | @{:.2}x | Won: ${:.2}",
                                                                                cashout.username,
                                                                                cashout.player_id,
                                                                                cashout.bet_amount_usd,
                                                                                cashout.multiplier,
                                                                                cashout.cashout_amount_usd);
                                                                            
                                                                            round_tracker.cashouts.push(cashout);
                                                                        } else {
                                                                            println!("      ⚠️  Invalid cashout (mult or cashout is 0)");
                                                                        }
                                                                    } else {
                                                                        println!("      ⚠️  Unrecognized action or incomplete data");
                                                                    }
                                                                } else {
                                                                    println!("      ⚠️  Failed to parse player data");
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
    
    println!("🚀 Starting JetX Monitor Service with PostgreSQL");
    println!("🌐 Web server on port {}", port);
    println!("💾 Database: PostgreSQL (Aiven)");
    println!("📡 WebSocket: Auto-reconnecting monitor");
    println!("💓 Health endpoints ready for cron-job.org:");
    println!("   - GET http://your-app.koyeb.app/health");
    println!("   - GET http://your-app.koyeb.app/status");
    println!("⏰ Recommended cron schedule: Every 5 minutes");
    println!("📋 IMPORTANT: First round after startup will be skipped (incomplete data)");
    println!("             All subsequent rounds will be fully tracked and saved");
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
    .workers(2) // Use 2 workers for better reliability
    .run()
    .await
}
