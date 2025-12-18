I have put this endpoint on my app. is it okay:
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{StreamExt, SinkExt};
use serde_json::Value;
use std::env;
use actix_web::{get, App, HttpResponse, HttpServer, Responder};
use reqwest;

#[derive(Debug, PartialEq)]
enum GameState {
    AcceptingBets,
    PlaneStarted,
    Flying,
    Crashed,
    BoardLoaded,
}

struct RoundStats {
    total_bets_usd: f64,
    total_cashouts_usd: f64,
    bet_leak_shown: bool,
    cashouts: Vec<CashoutInfo>,
}

struct CashoutInfo {
    username: String,
    bet_usd: f64,
    bet_local: String,
    currency: String,
    multiplier: f64,
    won_usd: f64,
}

impl RoundStats {
    fn new() -> Self {
        RoundStats {
            total_bets_usd: 0.0,
            total_cashouts_usd: 0.0,
            bet_leak_shown: false,
            cashouts: Vec::new(),
        }
    }
    
    fn reset(&mut self) {
        self.total_bets_usd = 0.0;
        self.total_cashouts_usd = 0.0;
        self.bet_leak_shown = false;
        self.cashouts.clear();
    }
    
    fn add_cashout(&mut self, username: String, bet_usd: f64, bet_local: String, currency: String, multiplier: f64, won_usd: f64) {
        self.total_cashouts_usd += won_usd;
        self.cashouts.push(CashoutInfo {
            username,
            bet_usd,
            bet_local,
            currency,
            multiplier,
            won_usd,
        });
    }
    
    fn print_sorted_cashouts(&mut self) {
        // Sort by multiplier in ascending order
        self.cashouts.sort_by(|a, b| a.multiplier.partial_cmp(&b.multiplier).unwrap());
        
        if !self.cashouts.is_empty() {
            println!("\n💸 CASHOUTS (sorted by multiplier - ascending order):");
            println!("{}", "-".repeat(80));
            for cashout in &self.cashouts {
                println!("   {} | Bet: ${} ({} {}) | Cashed out at: {:.2}x | Won: ${}", 
                    cashout.username, cashout.bet_usd, cashout.bet_local, 
                    cashout.currency, cashout.multiplier, cashout.won_usd);
            }
            println!("{}", "-".repeat(80));
        }
    }
}

#[get("/")]
async fn hello() -> impl Responder {
    println!("📡 Health check received at /");
    HttpResponse::Ok().body("JetX WebSocket Monitor is running!")
}

#[get("/health")]
async fn health() -> impl Responder {
    println!("📡 Health check received at /health");
    HttpResponse::Ok().body("OK")
}

#[get("/status")]
async fn status() -> impl Responder {
    println!("📡 Status check received");
    use std::time::SystemTime;
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    
    HttpResponse::Ok().json(serde_json::json!({
        "status": "running",
        "service": "jetx-monitor",
        "timestamp": timestamp
    }))
}

async fn run_websocket_monitor() {
    let mut reconnect_delay = 5;
    loop {
        println!("\n🔄 Starting WebSocket monitor...");
        match monitor_jetx().await {
            Ok(_) => {
                println!("WebSocket connection closed normally");
                reconnect_delay = 5; // Reset delay on normal close
            }
            Err(e) => {
                eprintln!("❌ WebSocket error: {}. Reconnecting in {} seconds...", e, reconnect_delay);
            }
        }
        
        tokio::time::sleep(tokio::time::Duration::from_secs(reconnect_delay)).await;
        
        // Don't increase delay too much to ensure quick reconnection
        if reconnect_delay < 30 {
            reconnect_delay += 5;
        }
    }
}

// Heartbeat to keep service active and prevent Koyeb from sleeping
async fn heartbeat() {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30)); // Every 30 seconds
    interval.tick().await; // Skip first tick
    
    loop {
        interval.tick().await;
        println!("💓 Heartbeat: Service is active and running");
    }
}

async fn monitor_jetx() -> Result<(), Box<dyn std::error::Error>> {
    let ws_url = env::var("WS_URL").unwrap_or_else(|_| 
        "wss://eu-server-w4.ssgportal.com/JetXNode703/signalr/connect?transport=webSockets&clientProtocol=1.5&token=9b781e6b-66cf-4c27-af1d-108a66f7c77a&group=JetX&connectionToken=gY20tsEI3EJ0d4LQFAoTCVLqD7ZmXiC%2FZKHqy67RJLm7pukW%2FaEg22lUMCwxZNJNP0OtdK8c9KX%2FW21OXTaG0SlLHbivEAUDGR%2FcbMLrNI3pCz2dZwvVjBj%2BcjmyBNe2&connectionData=%5B%7B%22name%22%3A%22h%22%7D%5D&tid=0".to_string()
    );
    
    println!("🚀 Connecting to JetX WebSocket...");
    println!("{}", "=".repeat(80));
    
    let (ws_stream, _response) = connect_async(&ws_url).await?;
    println!("✅ Connected to JetX!\n");
    
    let (mut write, mut read) = ws_stream.split();
    
    let mut game_state = GameState::AcceptingBets;
    let mut round_counter = 0;
    let mut round_stats = RoundStats::new();
    let mut ping_counter = 0u32;
    let mut last_activity = tokio::time::Instant::now();
    
    // Get port for self-ping
    let port = env::var("PORT").unwrap_or("8000".to_string());
    let health_url = format!("http://localhost:{}/health", port);
    
    // Spawn timer ping task - send every 2 seconds
    let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<u32>(100);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(2));
        interval.tick().await; // Skip first immediate tick
        
        loop {
            interval.tick().await;
            if ping_tx.send(1).await.is_err() {
                break;
            }
        }
    });
    
    // Spawn HTTP self-ping task - ping our own health endpoint every 60 seconds
    let health_url_clone = health_url.clone();
    let (http_ping_tx, mut http_ping_rx) = tokio::sync::mpsc::channel::<()>(100);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        interval.tick().await;
        
        loop {
            interval.tick().await;
            if http_ping_tx.send(()).await.is_err() {
                break;
            }
        }
    });
    
    // Spawn inactivity checker - reconnect if no data for 120 seconds
    let (activity_tx, mut activity_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            if activity_tx.send(()).await.is_err() {
                break;
            }
        }
    });
    
    loop {
        tokio::select! {
            // Handle incoming messages
            message = read.next() => {
                last_activity = tokio::time::Instant::now();
                
                match message {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(json) = serde_json::from_str::<Value>(&text) {
                            // Check for TimerPing response (I and R fields)
                            if json.get("I").is_some() && json.get("R").is_some() {
                                let i_value = json["I"].as_str().or_else(|| json["I"].as_u64().map(|v| v.to_string().leak() as &str)).unwrap_or("?");
                                if let Some(r_obj) = json["R"].as_object() {
                                    if let Some(o_value) = r_obj.get("o") {
                                        println!("🏓 TimerPing Response: I: \"{}\", R: {{o: {}}}", i_value, o_value);
                                        continue;
                                    }
                                }
                            }
                            
                            // Check if message contains multiple items
                            let has_multiple = json["M"].as_array()
                                .map(|arr| arr.len() > 1)
                                .unwrap_or(false);
                            
                            if has_multiple {
                                println!("\n{}", "─".repeat(80));
                                println!("📦 PACKET START (Multiple messages in one packet)");
                                println!("{}", "─".repeat(80));
                            }
                            
                            process_message(&json, &mut game_state, &mut round_counter, &mut round_stats);
                            
                            if has_multiple {
                                println!("{}", "─".repeat(80));
                                println!("📦 PACKET END");
                                println!("{}", "─".repeat(80));
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        write.send(Message::Pong(data)).await?;
                    }
                    Some(Ok(Message::Close(frame))) => {
                        println!("\n🔌 Connection closed by server: {:?}", frame);
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        eprintln!("❌ Error receiving message: {}", e);
                        return Err(Box::new(e));
                    }
                    None => {
                        println!("\n🔌 Connection stream ended");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            
            // Handle sending TimerPing every 2 seconds
            _ = ping_rx.recv() => {
                ping_counter += 1;
                let ping_msg = serde_json::json!({
                    "H": "h",
                    "M": "TimerPing",
                    "A": [format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", 
                        rand::random::<u32>(), 
                        rand::random::<u16>(), 
                        rand::random::<u16>(), 
                        rand::random::<u16>(), 
                        rand::random::<u64>() & 0xFFFFFFFFFFFF)],
                    "I": ping_counter
                });
                
                let ping_str = serde_json::to_string(&ping_msg)?;
                println!("🏓 Sending TimerPing: I: {}", ping_counter);
                
                if let Err(e) = write.send(Message::Text(ping_str)).await {
                    eprintln!("❌ Error sending ping: {}", e);
                    return Err(Box::new(e));
                }
            }
            
            // Handle HTTP self-ping every 60 seconds to keep Koyeb active
            _ = http_ping_rx.recv() => {
                match reqwest::get(&health_url_clone).await {
                    Ok(_) => println!("💓 HTTP keep-alive ping successful - preventing sleep"),
                    Err(e) => eprintln!("⚠️  HTTP keep-alive ping failed: {}", e),
                }
            }
            
            // Check for inactivity timeout
            _ = activity_rx.recv() => {
                let inactive_duration = tokio::time::Instant::now().duration_since(last_activity);
                if inactive_duration > tokio::time::Duration::from_secs(120) {
                    println!("\n⚠️  No activity for 120 seconds, reconnecting...");
                    return Ok(());
                }
            }
        }
    }
}

fn process_message(json: &Value, game_state: &mut GameState, round_counter: &mut i32, round_stats: &mut RoundStats) {
    if let Some(messages) = json["M"].as_array() {
        for msg in messages {
            let method = msg["M"].as_str().unwrap_or("");
            
            match method {
                "response" => {
                    if let Some(args) = msg["A"].as_array() {
                        for arg in args {
                            process_flight_data(arg, game_state, round_counter, round_stats);
                        }
                    }
                }
                "g" => {
                    if let Some(args) = msg["A"].as_array() {
                        for arg in args {
                            process_bet_data(arg, game_state, round_stats);
                        }
                    }
                }
                "gBoard" => {
                    if *game_state == GameState::Crashed {
                        println!("\n📊 gBoard message received - Round data loaded");
                        println!("{}", "=".repeat(80));
                        println!("⚠️  Any CASHOUTS after this point are DATA LEAKS:");
                        println!("{}", "-".repeat(80));
                        round_stats.reset();
                        *game_state = GameState::AcceptingBets;
                    }
                }
                "TimerPing" => {}
                _ => {
                    if method != "" {
                        println!("\n⚠️  Unknown message type: {}", method);
                        println!("{}", serde_json::to_string_pretty(&msg).unwrap_or_default());
                    }
                }
            }
        }
    }
}

fn process_flight_data(data: &Value, game_state: &mut GameState, round_counter: &mut i32, round_stats: &mut RoundStats) {
    let fallen = data["f"].as_bool().unwrap_or(false);
    let multiplier = data["v"].as_f64().unwrap_or(0.0);
    let time = data["s"].as_f64().unwrap_or(0.0);
    
    if multiplier == 1.0 && time == 0.0 && !fallen {
        if *game_state == GameState::AcceptingBets {
            *round_counter += 1;
            *game_state = GameState::PlaneStarted;
            
            println!("\n{}", "=".repeat(80));
            println!("📊 BETTING PHASE COMPLETE");
            println!("   Total Bets Placed (including any leaks): ${:.2}", round_stats.total_bets_usd);
            println!("{}", "=".repeat(80));
            
            println!("\n🛫 ROUND #{} - PLANE HAS STARTED! (v=1, s=0)", round_counter);
            println!("{}", "=".repeat(80));
        }
        return;
    }
    
    if !fallen && (*game_state == GameState::PlaneStarted || *game_state == GameState::Flying) {
        if *game_state == GameState::PlaneStarted {
            *game_state = GameState::Flying;
        }
        println!("✈️  Multiplier: {:.2}x | Time: {:.2}s | Status: FLYING", multiplier, time);
        return;
    }
    
    if fallen {
        if *game_state == GameState::Flying || *game_state == GameState::PlaneStarted {
            *game_state = GameState::Crashed;
            println!("\n💥 PLANE CRASHED!");
            println!("   Crash Multiplier: {:.2}x", multiplier);
            println!("   Flight Time: {:.2}s", time);
            
            round_stats.print_sorted_cashouts();
            
            println!("\n📊 ROUND SUMMARY:");
            println!("   Total Bets Placed: ${:.2}", round_stats.total_bets_usd);
            println!("   Total Cashouts: ${:.2}", round_stats.total_cashouts_usd);
            println!("{}", "=".repeat(80));
        } else {
            println!("⚠️  [DATA LEAK] Received crash message after round ended (v={:.2}x, s={:.2}s)", multiplier, time);
        }
        return;
    }
}

fn process_bet_data(data: &Value, game_state: &mut GameState, round_stats: &mut RoundStats) {
    if let Some(bet_type) = data["M"].as_str() {
        if let Some(info) = data["I"]["a"].as_str() {
            let parts: Vec<&str> = info.split('_').collect();
            
            if parts.len() >= 9 {
                let username = parts[0];
                let bet_usd = parts[1].parse::<f64>().unwrap_or(0.0);
                let bet_local = parts[2];
                let cashout_multiplier = parts[3].parse::<f64>().unwrap_or(0.0);
                let cashout_usd = parts[4].parse::<f64>().unwrap_or(0.0);
                let random_id = parts[5];
                let _flag = parts[6];
                let currency = parts[7];
                let _status = parts[8];
                
                match bet_type {
                    "b" => {
                        if cashout_multiplier == 0.0 && cashout_usd == 0.0 {
                            round_stats.total_bets_usd += bet_usd;
                            
                            if *game_state == GameState::AcceptingBets {
                                println!("💰 BET PLACED: {} | ${} ({} {}) [ID: {}] → Total Bets: ${:.2}", 
                                    username, bet_usd, bet_local, currency, random_id, round_stats.total_bets_usd);
                            } 
                            else {
                                if !round_stats.bet_leak_shown {
                                    println!("\n{}", "=".repeat(80));
                                    println!("⚠️  BET DATA LEAKS (Bets placed after plane started):");
                                    println!("{}", "-".repeat(80));
                                    round_stats.bet_leak_shown = true;
                                }
                                println!("⚠️  [BET LEAK] {} | ${} ({} {}) [ID: {}] → Total Bets: ${:.2}", 
                                    username, bet_usd, bet_local, currency, random_id, round_stats.total_bets_usd);
                            }
                        }
                    }
                    "c" => {
                        if cashout_multiplier > 1.0 && cashout_usd > 0.0 {
                            if *game_state == GameState::Flying {
                                round_stats.add_cashout(
                                    username.to_string(),
                                    bet_usd,
                                    bet_local.to_string(),
                                    currency.to_string(),
                                    cashout_multiplier,
                                    cashout_usd
                                );
                                println!("💸 Cashout detected: {} at {:.2}x", username, cashout_multiplier);
                            } 
                            else if *game_state == GameState::Crashed || *game_state == GameState::AcceptingBets {
                                println!("⚠️  [DATA LEAK - CASHOUT] {} | Bet: ${} | Multiplier: {:.2}x | Won: ${} [ID: {}]", 
                                    username, bet_usd, cashout_multiplier, cashout_usd, random_id);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = env::var("PORT")
        .unwrap_or("8000".to_string())
        .parse()
        .unwrap();

    println!("🌐 Starting web server on port {}", port);
    println!("💓 Heartbeat enabled - service will stay active");
    
    // Spawn WebSocket monitor as a background task
    tokio::spawn(async {
        run_websocket_monitor().await;
    });
    
    // Spawn heartbeat to keep service active
    tokio::spawn(async {
        heartbeat().await;
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
