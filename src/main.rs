use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug, PartialEq)]
enum GameState {
    AcceptingBets, // Before plane starts - normal betting period
    PlaneStarted,  // After v=1, s=0 - betting should stop
    Flying,
    Crashed,
    BoardLoaded, // After gBoard message - round truly over
}

struct RoundStats {
    total_bets_usd: f64,
    total_cashouts_usd: f64,
    bet_leak_shown: bool,
    cashouts: Vec<CashoutInfo>, // Store cashouts to sort later
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

    fn add_cashout(
        &mut self,
        username: String,
        bet_usd: f64,
        bet_local: String,
        currency: String,
        multiplier: f64,
        won_usd: f64,
    ) {
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
        self.cashouts
            .sort_by(|a, b| a.multiplier.partial_cmp(&b.multiplier).unwrap());

        if !self.cashouts.is_empty() {
            println!("\n💸 CASHOUTS (sorted by multiplier - ascending order):");
            println!("{}", "-".repeat(80));
            for cashout in &self.cashouts {
                println!(
                    "   {} | Bet: ${} ({} {}) | Cashed out at: {:.2}x | Won: ${}",
                    cashout.username,
                    cashout.bet_usd,
                    cashout.bet_local,
                    cashout.currency,
                    cashout.multiplier,
                    cashout.won_usd
                );
            }
            println!("{}", "-".repeat(80));
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ws_url = "wss://eu-server-w4.ssgportal.com/JetXNode703/signalr/connect?transport=webSockets&clientProtocol=1.5&token=9b781e6b-66cf-4c27-af1d-108a66f7c77a&group=JetX&connectionToken=gY20tsEI3EJ0d4LQFAoTCVLqD7ZmXiC%2FZKHqy67RJLm7pukW%2FaEg22lUMCwxZNJNP0OtdK8c9KX%2FW21OXTaG0SlLHbivEAUDGR%2FcbMLrNI3pCz2dZwvVjBj%2BcjmyBNe2&connectionData=%5B%7B%22name%22%3A%22h%22%7D%5D&tid=0";

    println!("🚀 Connecting to JetX WebSocket...");
    println!("{}", "=".repeat(80));

    let (ws_stream, _response) = connect_async(ws_url).await?;
    println!("✅ Connected to JetX!\n");

    let (mut write, mut read) = ws_stream.split();

    let mut game_state = GameState::AcceptingBets;
    let mut round_counter = 0;
    let mut round_stats = RoundStats::new();

    while let Some(message) = read.next().await {
        match message {
            Ok(Message::Text(text)) => {
                if let Ok(json) = serde_json::from_str::<Value>(&text) {
                    // Check if message contains multiple items
                    let has_multiple = json["M"]
                        .as_array()
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
            Ok(Message::Ping(data)) => {
                write.send(Message::Pong(data)).await?;
            }
            Ok(Message::Close(frame)) => {
                println!("\n🔌 Connection closed: {:?}", frame);
                break;
            }
            Err(e) => {
                eprintln!("❌ Error: {}", e);
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

fn process_message(
    json: &Value,
    game_state: &mut GameState,
    round_counter: &mut i32,
    round_stats: &mut RoundStats,
) {
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
                    // Board loaded - marks end of round data (only for cashout leaks)
                    if *game_state == GameState::Crashed {
                        println!("\n📊 gBoard message received - Round data loaded");
                        println!("{}", "=".repeat(80));
                        println!("⚠️  Any CASHOUTS after this point are DATA LEAKS:");
                        println!("{}", "-".repeat(80));
                        // Reset stats for next round
                        round_stats.reset();
                        // Set state back to accepting bets for next round
                        *game_state = GameState::AcceptingBets;
                    }
                }
                "TimerPing" => {
                    // Ignore timer pings
                }
                _ => {
                    // Print unknown messages for debugging
                    if method != "" {
                        println!("\n⚠️  Unknown message type: {}", method);
                        println!("{}", serde_json::to_string_pretty(&msg).unwrap_or_default());
                    }
                }
            }
        }
    }
}

fn process_flight_data(
    data: &Value,
    game_state: &mut GameState,
    round_counter: &mut i32,
    round_stats: &mut RoundStats,
) {
    let fallen = data["f"].as_bool().unwrap_or(false);
    let multiplier = data["v"].as_f64().unwrap_or(0.0);
    let time = data["s"].as_f64().unwrap_or(0.0);

    // Plane just started (v=1, s=0, f=false)
    if multiplier == 1.0 && time == 0.0 && !fallen {
        if *game_state == GameState::AcceptingBets {
            *round_counter += 1;
            *game_state = GameState::PlaneStarted;

            // Print total bets before flight starts
            println!("\n{}", "=".repeat(80));
            println!("📊 BETTING PHASE COMPLETE");
            println!(
                "   Total Bets Placed (including any leaks): ${:.2}",
                round_stats.total_bets_usd
            );
            println!("{}", "=".repeat(80));

            println!(
                "\n🛫 ROUND #{} - PLANE HAS STARTED! (v=1, s=0)",
                round_counter
            );
            println!("{}", "=".repeat(80));
        }
        return;
    }

    // Plane is flying (multiplier increases, f=false)
    if !fallen && (*game_state == GameState::PlaneStarted || *game_state == GameState::Flying) {
        if *game_state == GameState::PlaneStarted {
            *game_state = GameState::Flying;
        }
        println!(
            "✈️  Multiplier: {:.2}x | Time: {:.2}s | Status: FLYING",
            multiplier, time
        );
        return;
    }

    // Plane crashed (f=true)
    if fallen {
        if *game_state == GameState::Flying || *game_state == GameState::PlaneStarted {
            *game_state = GameState::Crashed;
            println!("\n💥 PLANE CRASHED!");
            println!("   Crash Multiplier: {:.2}x", multiplier);
            println!("   Flight Time: {:.2}s", time);

            // Print sorted cashouts
            round_stats.print_sorted_cashouts();

            println!("\n📊 ROUND SUMMARY:");
            println!("   Total Bets Placed: ${:.2}", round_stats.total_bets_usd);
            println!("   Total Cashouts: ${:.2}", round_stats.total_cashouts_usd);
            println!("{}", "=".repeat(80));
        } else {
            // Data leak: crash message after we already processed the crash
            println!(
                "⚠️  [DATA LEAK] Received crash message after round ended (v={:.2}x, s={:.2}s)",
                multiplier, time
            );
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
                        // Bet placed: bet_amount > 0, multiplier = 0, cashout = 0
                        if cashout_multiplier == 0.0 && cashout_usd == 0.0 {
                            // Always add to total bets (including leaks)
                            round_stats.total_bets_usd += bet_usd;

                            // Valid bet: before plane starts
                            if *game_state == GameState::AcceptingBets {
                                println!(
                                    "💰 BET PLACED: {} | ${} ({} {}) [ID: {}] → Total Bets: ${:.2}",
                                    username,
                                    bet_usd,
                                    bet_local,
                                    currency,
                                    random_id,
                                    round_stats.total_bets_usd
                                );
                            }
                            // Data leak: bet after v=1, s=0
                            else {
                                // Show separator line only once when first bet leak occurs
                                if !round_stats.bet_leak_shown {
                                    println!("\n{}", "=".repeat(80));
                                    println!(
                                        "⚠️  BET DATA LEAKS (Bets placed after plane started):"
                                    );
                                    println!("{}", "-".repeat(80));
                                    round_stats.bet_leak_shown = true;
                                }
                                println!(
                                    "⚠️  [BET LEAK] {} | ${} ({} {}) [ID: {}] → Total Bets: ${:.2}",
                                    username,
                                    bet_usd,
                                    bet_local,
                                    currency,
                                    random_id,
                                    round_stats.total_bets_usd
                                );
                            }
                        }
                    }
                    "c" => {
                        // Cashout: multiplier > 1, cashout_usd > 0
                        if cashout_multiplier > 1.0 && cashout_usd > 0.0 {
                            if *game_state == GameState::Flying {
                                round_stats.add_cashout(
                                    username.to_string(),
                                    bet_usd,
                                    bet_local.to_string(),
                                    currency.to_string(),
                                    cashout_multiplier,
                                    cashout_usd,
                                );
                                // Just acknowledge the cashout, don't print details yet
                                println!(
                                    "💸 Cashout detected: {} at {:.2}x",
                                    username, cashout_multiplier
                                );
                            }
                            // Data leak: cashout after crash (including after gBoard)
                            else if *game_state == GameState::Crashed
                                || *game_state == GameState::AcceptingBets
                            {
                                println!(
                                    "⚠️  [DATA LEAK - CASHOUT] {} | Bet: ${} | Multiplier: {:.2}x | Won: ${} [ID: {}]",
                                    username, bet_usd, cashout_multiplier, cashout_usd, random_id
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
//pending: ping every after 2s and it should start a counter
/*6) These are the pings sent to the server:
H: "h", M: "TimerPing", A: ["2f4d4f45-529e-4c7f-89c2-01ed6e18bcc7"], I: 3}
A
:
["2f4d4f45-529e-4c7f-89c2-01ed6e18bcc7"]
0
:
"2f4d4f45-529e-4c7f-89c2-01ed6e18bcc7"
H
:
"h"
I
:
3
M
:
"TimerPing"
I is the counter and it should start from 1 whever the first ping is sent to the server
This is how the response looks like which is the pong:
R: {o: 405}, I: "3"}
I
:
"3"
R
:
{o: 405}
o
:
405
Again, I is the counter,; in this context, the ping
*/
