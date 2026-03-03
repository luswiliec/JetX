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
use reqwest;
use url::form_urlencoded;

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

// NEW: Function to get fresh SignalR connection token
async fn get_signalr_token() -> Result<String, Box<dyn Error>> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    // Step 1: Negotiate connection
    let negotiate_url = "https://eu-server-w15.ssgportal.com/JetXNode703/signalr/negotiate?clientProtocol=1.5&connectionData=%5B%7B%22name%22%3A%22h%22%7D%5D";
    
    println!("🔑 Requesting fresh SignalR token...");
    
    let response = client
        .get(negotiate_url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("Negotiate failed: {}", response.status()).into());
    }

    let json: Value = response.json().await?;
    
    let connection_token = json["ConnectionToken"]
        .as_str()
        .ok_or("No ConnectionToken in response")?;
    
    let connection_id = json["ConnectionId"]
        .as_str()
        .ok_or("No ConnectionId in response")?;

    println!("✅ Got fresh token (Connection ID: {})", connection_id);

    // URL encode the connection token
    let encoded_token: String = form_urlencoded::byte_serialize(connection_token.as_bytes()).collect();

    Ok(encoded_token)
}

// Database connection function
async fn get_db_client() -> Result<Client, Box<dyn Error>> {
    let db_user = env::var("DATABASE_USER").unwrap_or("avnadmin".to_string());
    let db_password = env::var("DATABASE_PASSWORD").unwrap_or("AVNS_qo4RbZtZ5nTmv6oZCvL".to_string());
    let db_host = env::var("DATABASE_HOST").unwrap_or("pg-406c52b-luswiliec-transcity.k.aivencloud.com".to_string());
    let db_port = env::var("DATABASE_PORT").unwrap_or("12394".to_string());
    let db_name = env::var("DATABASE_NAME").unwrap_or("defaultdb".to_string());

    let mut builder = native_tls::TlsConnector::builder();
    builder.danger_accept_invalid_certs(true);
    let tls_connector = builder.build()?;
    let tls = postgres_native_tls::MakeTlsConnector::new(tls_connector);

    let connection_string = format!(
        "host={} port={} user={} password={} dbname={} sslmode=require",
        db_host, db_port, db_user, db_password, db_name
    );

    println!("🔌 Connecting to PostgreSQL...");

    let (client, connection) = tokio_postgres::connect(&connection_string, tls).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("❌ Database connection error: {}", e);
        }
    });

    println!("✅ Database connected successfully");
    
    Ok(client)
}

async fn save_round_to_db(client: &Client, round: &GameRound) -> Result<(), Box<dyn Error>> {
    let query = "
        INSERT INTO jetxv1 (
            date, time, crash_multiplier, flight_duration, 
            total_bets_usd, total_players_bet, total_cashouts_usd, 
            total_players_cashed_out, profit_usd, players_lost
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
    ";

    client.execute(
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
    ).await?;

    println!("💾 Saved to database successfully");
    Ok(())
}

#[get("/")]
async fn hello() -> impl Responder {
    let adjusted_time = get_adjusted_time();
    HttpResponse::Ok()
        .content_type("application/json")
        .json(serde_json::json!({
            "status": "ok",
            "service": "JetX Game Data Monitor",
            "message": "Service is running and monitoring JetX games",
            "timestamp": adjusted_time.to_rfc3339(),
        }))
}

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok()
        .content_type("application/json")
        .json(serde_json::json!({
            "status": "healthy",
        }))
}

#[get("/status")]
async fn status() -> impl Responder {
    HttpResponse::Ok()
        .content_type("application/json")
        .json(serde_json::json!({
            "status": "monitoring",
        }))
}

async fn run_websocket_monitor() {
    let mut reconnect_attempts = 0;
    loop {
        reconnect_attempts += 1;
        println!("\n🔄 WebSocket Monitor Attempt #{}", reconnect_attempts);
        
        match monitor_jetx().await {
            Ok(_) => {
                println!("⚠️  WebSocket connection ended. Reconnecting in 5 seconds...");
                reconnect_attempts = 0;
            }
            Err(e) => {
                eprintln!("❌ WebSocket error: {}. Reconnecting in 5 seconds...", e);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

async fn monitor_jetx() -> Result<(), Box<dyn Error>> {
    // Get fresh token
    let connection_token = get_signalr_token().await?;

    // Build WebSocket URL with fresh token
    let ws_url = format!(
        "wss://eu-server-w15.ssgportal.com/JetXNode703/signalr/connect?transport=webSockets&clientProtocol=1.5&connectionToken={}&connectionData=%5B%7B%22name%22%3A%22h%22%7D%5D",
        connection_token
    );

    println!("🔌 Connecting to WebSocket...");

    let connector = NativeTlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()?;
    let connector = Connector::NativeTls(connector);
    
    let (ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
        &ws_url,
        None,
        false,
        Some(connector)
    ).await?;
    
    println!("✅ WebSocket connection established");

    let db_client = get_db_client().await?;
    let (mut write, mut read) = ws_stream.split();

    println!("📊 Listening for JetX game data...");
    println!("{}", "=".repeat(80));

    let mut round_tracker = RoundTracker::new();
    let mut round_count = 0;
    let mut first_round_seen = false;

    while let Some(message) = read.next().await {
        match message {
            Ok(msg) => {
                match msg {
                    Message::Text(text) => {
                        if let Ok(json) = serde_json::from_str::<Value>(&text) {
                            if let Some(messages) = json["M"].as_array() {
                                for msg_obj in messages.iter() {
                                    if let Some(method) = msg_obj["M"].as_str() {
                                        if method == "response" {
                                            if let Some(args) = msg_obj["A"].as_array() {
                                                if let Some(arg) = args.first() {
                                                    let f = arg["f"].as_bool().unwrap_or(false);
                                                    let v = arg["v"].as_f64().unwrap_or(0.0);
                                                    let s = arg["s"].as_f64().unwrap_or(0.0);

                                                    if !f && v == 1.0 && s == 0.0 && !round_tracker.is_active {
                                                        round_tracker.start_time = Some(get_adjusted_time());
                                                        round_tracker.is_active = true;
                                                        round_count += 1;
                                                        println!("\n🚀 [ROUND {}] FLIGHT STARTED", round_count);
                                                    } else if !f && round_tracker.is_active {
                                                        round_tracker.crash_multiplier = v;
                                                        round_tracker.flight_duration = s;
                                                    } else if f && round_tracker.is_active {
                                                        round_tracker.crash_multiplier = v;
                                                        round_tracker.flight_duration = s;
                                                        println!("\n💥 CRASHED at {:.2}x", v);

                                                        let round_stats = round_tracker.calculate_stats();
                                                        
                                                        if first_round_seen {
                                                            if let Err(e) = save_round_to_db(&db_client, &round_stats).await {
                                                                eprintln!("❌ Database error: {}", e);
                                                            }
                                                        } else {
                                                            println!("⚠️  SKIPPED (First round)");
                                                            first_round_seen = true;
                                                        }

                                                        round_tracker.reset();
                                                    }
                                                }
                                            }
                                        } else if method == "g" {
                                            if let Some(args) = msg_obj["A"].as_array() {
                                                if let Some(arg) = args.first() {
                                                    if let Some(action_type) = arg["M"].as_str() {
                                                        if let Some(info) = arg["I"].as_object() {
                                                            if let Some(data) = info.get("a").and_then(|v| v.as_str()) {
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
                                                                            round_tracker.bets.insert(key, bet);
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
                                                                            
                                                                            round_tracker.cashouts.push(cashout);
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    Message::Ping(data) => {
                        write.send(Message::Pong(data)).await?;
                    }

                    Message::Close(_) => {
                        println!("\n[CLOSE] Connection closed by server");
                        break;
                    }

                    _ => {}
                }
            }

            Err(e) => {
                return Err(e.into());
            }
        }
    }
    
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
    
    tokio::spawn(async {
        run_websocket_monitor().await;
    });
    
    HttpServer::new(|| {
        App::new()
            .service(hello)
            .service(health)
            .service(status)
    })
    .bind(("0.0.0.0", port))?
    .workers(2)
    .run()
    .await
                                                        }
