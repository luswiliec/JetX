// =============================================================================
// JetX Data Scraper — Rust WebSocket Client
// Parses all message cases (1-4), detects leaks, prints round reports
// =============================================================================

use colored::*;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::VecDeque;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

// ── Default WebSocket URL (replace token before running) ────────────────────
const WS_URL: &str = "wss://eu-server-w16.ssgportal.com/JetXNode703/signalr/connect\
    ?transport=webSockets\
    &clientProtocol=1.5\
    &token=34c05fbd-299a-4ec1-836f-c82f43aabc93\
    &group=JetX\
    &connectionToken=ZxOc97e7lQqiC1vpV%2F%2BsjyFe8yYwy8uxvUF%2BefISS1505gnV3Ev3Jy\
    %2BY1n3wCBjQPMWjJgLNI0RwQ7I9wJuHDwq%2Fx5%2BYmkKIed%2BmC8yQtmw2u%2Bm42vg5BFPizszhdIs0\
    &connectionData=%5B%7B%22name%22%3A%22h%22%7D%5D\
    &tid=4";

// ── Data Structures ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CashoutEvent {
    counter:     String,
    player_name: String,
    bet_usd:     f64,
    bet_local:   f64,
    multiplier:  f64,
    cashout_usd: f64,
    currency:    String,
    slot:        u8,
    player_id:   String,
}

#[derive(Debug, Clone)]
struct BetEvent {
    counter:     String,
    player_name: String,
    bet_usd:     f64,
    bet_local:   f64,
    currency:    String,
    slot:        u8,
    player_id:   String,
}

#[derive(Debug, Clone)]
struct RoundResult {
    crash_multiplier: f64,
    flight_time:      f64,
    normal_bets:      Vec<BetEvent>,
    normal_cashouts:  Vec<CashoutEvent>,
    leaked_bets:      Vec<BetEvent>,
    leaked_cashouts:  Vec<CashoutEvent>,
}

// ── Game State Machine ────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Clone)]
enum GameState {
    WaitingForBets,   // before plane lifts off; Case 4 bets are normal here
    Flying,           // plane is airborne; Case 4 bets are leaks here
    Crashed,          // f=true received; Case 2 after this are leaks
}

struct AppState {
    game_state:       GameState,
    round_number:     u32,
    last_counter:     Option<u64>,    // last hex counter as integer
    current_mult:     f64,
    current_time:     f64,
    // Current round data
    normal_bets:      Vec<BetEvent>,
    normal_cashouts:  Vec<CashoutEvent>,
    leaked_bets:      Vec<BetEvent>,
    leaked_cashouts:  Vec<CashoutEvent>,
    // History for prediction
    history:          VecDeque<f64>,
}

impl AppState {
    fn new() -> Self {
        Self {
            game_state:      GameState::WaitingForBets,
            round_number:    0,
            last_counter:    None,
            current_mult:    1.0,
            current_time:    0.0,
            normal_bets:     Vec::new(),
            normal_cashouts: Vec::new(),
            leaked_bets:     Vec::new(),
            leaked_cashouts: Vec::new(),
            history:         VecDeque::with_capacity(100),
        }
    }

    fn reset_round(&mut self) {
        self.round_number    += 1;
        self.game_state       = GameState::WaitingForBets;
        self.current_mult     = 1.0;
        self.current_time     = 0.0;
        self.normal_bets      .clear();
        self.normal_cashouts  .clear();
        self.leaked_bets      .clear();
        self.leaked_cashouts  .clear();
    }
}

// ── Counter Parsing ───────────────────────────────────────────────────────────

/// Extract hex counter string from the C field, e.g. "d-75BE2366-B,67B0C3A|yahM,0|yahN,1"
/// returns "67B0C3A" and its integer value
fn parse_counter(c_field: &str) -> Option<(String, u64)> {
    let comma_part = c_field.split(',').nth(1)?;
    let hex_str    = comma_part.split('|').next()?;
    let val        = u64::from_str_radix(hex_str.trim(), 16).ok()?;
    Some((hex_str.trim().to_string(), val))
}

/// Given current counter int and number of sub-messages, produce assigned counters
fn assign_sub_counters(current_val: u64, count: usize) -> Vec<String> {
    let base = current_val - (count as u64) + 1;
    (0..count)
        .map(|i| format!("{:X}", base + i as u64))
        .collect()
}

// ── Message Parsers ───────────────────────────────────────────────────────────

/// Parse a "c" (cashout) inner 'a' string:
/// "1****1_0.4631548000_2000_1.05_0.4863125400_3363921496_0_UGX_0"
fn parse_cashout(a: &str, counter: &str) -> Option<CashoutEvent> {
    let parts: Vec<&str> = a.split('_').collect();
    if parts.len() < 9 { return None; }
    Some(CashoutEvent {
        counter:     counter.to_string(),
        player_name: parts[0].to_string(),
        bet_usd:     parts[1].parse().ok()?,
        bet_local:   parts[2].parse().ok()?,
        multiplier:  parts[3].parse().ok()?,
        cashout_usd: parts[4].parse().ok()?,
        player_id:   parts[5].to_string(),
        // parts[6] = unknown int, skip
        currency:    parts[7].to_string(),
        slot:        parts[8].parse().unwrap_or(0),
    })
}

/// Parse a "b" (bet) inner 'a' string:
/// "B****D_0.066843264000_10.00_0_0_1238455989_0_KES_2"
fn parse_bet(a: &str, counter: &str) -> Option<BetEvent> {
    let parts: Vec<&str> = a.split('_').collect();
    if parts.len() < 9 { return None; }
    Some(BetEvent {
        counter:     counter.to_string(),
        player_name: parts[0].to_string(),
        bet_usd:     parts[1].parse().ok()?,
        bet_local:   parts[2].parse().ok()?,
        player_id:   parts[5].to_string(),
        currency:    parts[7].to_string(),
        slot:        parts[8].parse().unwrap_or(0),
    })
}

// ── Print Helpers ─────────────────────────────────────────────────────────────

fn separator(label: &str) {
    println!(
        "\n{}",
        format!("──── {} {}", label, "─".repeat(60usize.saturating_sub(label.len() + 6)))
            .bright_black()
    );
}

fn print_cashout(ev: &CashoutEvent, is_leak: bool) {
    let tag = if is_leak {
        "[LEAK-CASHOUT]".red().bold()
    } else {
        "[CASHOUT]     ".green().bold()
    };
    println!(
        "  {} {:>8} | Counter: {} | Bet: ${:.6} ({:.2} {}) | @{:.2}x → Cashed: ${:.6} | Slot: {} | ID: {}",
        tag,
        ev.player_name.cyan(),
        ev.counter.yellow(),
        ev.bet_usd,
        ev.bet_local,
        ev.currency,
        ev.multiplier,
        ev.cashout_usd,
        ev.slot,
        ev.player_id.bright_black(),
    );
}

fn print_bet(ev: &BetEvent, is_leak: bool) {
    let tag = if is_leak {
        "[LEAK-BET]    ".magenta().bold()
    } else {
        "[BET]         ".blue().bold()
    };
    println!(
        "  {} {:>8} | Counter: {} | Bet: ${:.6} ({:.2} {}) | Slot: {} | ID: {}",
        tag,
        ev.player_name.cyan(),
        ev.counter.yellow(),
        ev.bet_usd,
        ev.bet_local,
        ev.currency,
        ev.slot,
        ev.player_id.bright_black(),
    );
}

fn print_flight(mult: f64, time: f64, is_start: bool) {
    if is_start {
        println!(
            "  {} Plane lifted off — mult: {:.2}x  time: {:.2}s",
            "[CASE 1 START]".bright_cyan().bold(),
            mult, time
        );
    } else {
        print!(
            "\r  {} mult: {:.3}x  time: {:.2}s    ",
            "[FLYING]".bright_cyan(),
            mult, time
        );
        // flush stdout
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

fn print_crash(mult: f64, time: f64) {
    println!(
        "\n  {} Plane CRASHED at {:.2}x after {:.2}s",
        "[CASE 3 CRASH]".red().bold(),
        mult, time
    );
}

fn print_board_reset(round: u32) {
    println!(
        "  {} New round board reset → Round {}",
        "[gBOARD]".bright_black().bold(),
        round
    );
}

fn print_round_report(result: &RoundResult, history: &VecDeque<f64>) {
    println!("\n{}", "═".repeat(72).yellow());
    println!("{}", "  ROUND REPORT".yellow().bold());
    println!("{}", "═".repeat(72).yellow());

    // Crash info
    println!(
        "  Multiplier at crash : {:.2}x",
        result.crash_multiplier
    );
    println!("  Time of flight      : {:.2}s", result.flight_time);

    // ── Normal ──
    let n_bet_total:  f64 = result.normal_bets    .iter().map(|b| b.bet_usd    ).sum();
    let n_co_total:   f64 = result.normal_cashouts.iter().map(|c| c.cashout_usd).sum();
    println!("\n  {} Normal Data", "▶".green());
    println!("    Players that placed bets  : {}", result.normal_bets    .len().to_string().green());
    println!("    Players that cashed out   : {}", result.normal_cashouts.len().to_string().green());
    println!("    Total bets (USD)          : ${:.6}", n_bet_total);
    println!("    Total cashouts (USD)      : ${:.6}", n_co_total);

    // ── Leaked ──
    let l_bet_total:  f64 = result.leaked_bets    .iter().map(|b| b.bet_usd    ).sum();
    let l_co_total:   f64 = result.leaked_cashouts.iter().map(|c| c.cashout_usd).sum();
    println!("\n  {} Leaked Data", "▶".red());
    println!("    Leaked bet players        : {}", result.leaked_bets    .len().to_string().red());
    println!("    Leaked cashout players    : {}", result.leaked_cashouts.len().to_string().red());
    println!("    Leaked total bets (USD)   : ${:.6}", l_bet_total);
    println!("    Leaked total cashouts     : ${:.6}", l_co_total);

    // ── Sums ──
    let sum_bet_p  = result.normal_bets    .len() + result.leaked_bets    .len();
    let sum_co_p   = result.normal_cashouts.len() + result.leaked_cashouts.len();
    let sum_bets   = n_bet_total + l_bet_total;
    let sum_cos    = n_co_total  + l_co_total;
    println!("\n  {} Sum Total", "▶".bright_white());
    println!("    Sum players bet           : {}", sum_bet_p);
    println!("    Sum players cashed out    : {}", sum_co_p);
    println!("    Sum bets (USD)            : ${:.6}", sum_bets);
    println!("    Sum cashouts (USD)        : ${:.6}", sum_cos);

    // ── Highlights ──
    println!("\n  {} Highlights", "▶".bright_yellow());
    if let Some(top_bet) = result
        .normal_bets
        .iter()
        .max_by(|a, b| a.bet_usd.partial_cmp(&b.bet_usd).unwrap())
    {
        println!(
            "    Highest bettor            : {} — ${:.6} ({:.2} {})",
            top_bet.player_name.cyan(), top_bet.bet_usd, top_bet.bet_local, top_bet.currency
        );
    }
    if let Some(top_co) = result
        .normal_cashouts
        .iter()
        .max_by(|a, b| a.cashout_usd.partial_cmp(&b.cashout_usd).unwrap())
    {
        println!(
            "    Highest cashout           : {} — cashed ${:.6} (bet ${:.6} @{:.2}x)",
            top_co.player_name.cyan(), top_co.cashout_usd, top_co.bet_usd, top_co.multiplier
        );
    }

    // ── Historical prediction ──
    if history.len() >= 3 {
        let n = history.len() as f64;
        let mean: f64 = history.iter().sum::<f64>() / n;
        let mut sorted: Vec<f64> = history.iter().cloned().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q1  = sorted[(sorted.len() as f64 * 0.25) as usize];
        let med = sorted[(sorted.len() as f64 * 0.50) as usize];
        let q3  = sorted[(sorted.len() as f64 * 0.75) as usize];
        let p_below_2 = history.iter().filter(|&&v| v < 2.0).count() as f64 / n * 100.0;
        let p_below_3 = history.iter().filter(|&&v| v < 3.0).count() as f64 / n * 100.0;

        println!("\n  {} Prediction (n={})", "▶".bright_cyan(), history.len());
        println!("    Mean crash              : {:.2}x", mean);
        println!("    Median                  : {:.2}x", med);
        println!("    Q1 – Q3 (safe range)    : {:.2}x – {:.2}x", q1, q3);
        println!("    Crashed below 2x        : {:.1}%", p_below_2);
        println!("    Crashed below 3x        : {:.1}%", p_below_3);
        println!("    History (last {})       :", history.len());
        let hist_str: Vec<String> = history
            .iter()
            .rev()
            .take(20)
            .map(|v| {
                let s = format!("{:.2}x", v);
                if *v < 2.0 {
                    s.red().to_string()
                } else if *v < 5.0 {
                    s.yellow().to_string()
                } else {
                    s.green().to_string()
                }
            })
            .collect();
        println!("    {}", hist_str.join("  "));
    } else {
        println!(
            "\n  {} Prediction: Need at least 3 rounds (have {})",
            "▶".bright_cyan(),
            history.len()
        );
    }

    println!("{}\n", "═".repeat(72).yellow());
}

// ── Core Message Processor ────────────────────────────────────────────────────

fn process_message(raw: &str, state: &mut AppState) {
    let json: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return,
    };

    let c_field = json["C"].as_str().unwrap_or("");
    let messages = match json["M"].as_array() {
        Some(a) => a,
        None    => return,
    };

    if messages.is_empty() { return; }

    // Parse counter
    let (counter_str, counter_val) = match parse_counter(c_field) {
        Some(v) => v,
        None    => ("UNK".to_string(), 0),
    };

    // Assign sub-counters based on gap
    let sub_counters: Vec<String> = if messages.len() > 1 {
        assign_sub_counters(counter_val, messages.len())
    } else {
        vec![counter_str.clone()]
    };

    // Check for counter gap (informational)
    if let Some(last) = state.last_counter {
        let expected_next = last + 1;
        if counter_val > expected_next && messages.len() == 1 {
            let gap = counter_val - last - 1;
            println!(
                "  {} Counter gap detected: {} → {} (gap: {})",
                "[GAP]".bright_black(),
                format!("{:X}", last).yellow(),
                counter_str.yellow(),
                gap.to_string().red()
            );
        }
    }
    if counter_val > 0 {
        state.last_counter = Some(counter_val);
    }

    // Draw separator when multiple sub-messages
    if messages.len() > 1 {
        separator(&format!(
            "BATCH x{} @ {}",
            messages.len(),
            counter_str
        ));
    }

    // ── scan for crash first (to set crashed state before processing siblings) ──
    let crash_idx: Option<usize> = messages.iter().position(|m| {
        m["M"].as_str() == Some("response")
            && m["A"][0]["f"].as_bool() == Some(true)
    });

    // Track if we've passed a crash in THIS batch
    let mut crashed_in_batch = false;

    for (idx, msg) in messages.iter().enumerate() {
        let sub_ctr = sub_counters
            .get(idx)
            .cloned()
            .unwrap_or_else(|| counter_str.clone());

        let msg_type = msg["M"].as_str().unwrap_or("");

        match msg_type {
            // ─────────────────────────────────── gBoard (round reset)
            "gBoard" => {
                state.reset_round();
                print_board_reset(state.round_number);
            }

            // ─────────────────────────────────── response (fly / crash)
            "response" => {
                let a  = &msg["A"][0];
                let f  = a["f"].as_bool().unwrap_or(false);
                let v  = a["v"].as_f64().unwrap_or(1.0);
                let s  = a["s"].as_f64().unwrap_or(0.0);

                if !f {
                    // Flying update
                    state.current_mult = v;
                    state.current_time = s;
                    if v == 1.0 && s == 0.0 {
                        // Plane just took off
                        state.game_state = GameState::Flying;
                        print_flight(v, s, true);
                    } else {
                        print_flight(v, s, false);
                    }
                } else {
                    // CRASH
                    state.game_state = GameState::Crashed;
                    crashed_in_batch = true;
                    print_crash(v, s);

                    // Build result from what we have so far
                    let result = RoundResult {
                        crash_multiplier: v,
                        flight_time:      s,
                        normal_bets:      state.normal_bets     .clone(),
                        normal_cashouts:  state.normal_cashouts  .clone(),
                        leaked_bets:      state.leaked_bets      .clone(),
                        leaked_cashouts:  state.leaked_cashouts  .clone(),
                    };

                    // Keep history capped at 200
                    state.history.push_back(v);
                    if state.history.len() > 200 {
                        state.history.pop_front();
                    }

                    print_round_report(&result, &state.history);
                }
            }

            // ─────────────────────────────────── g (cashout or bet)
            "g" => {
                let inner_m = msg["A"][0]["M"].as_str().unwrap_or("");
                let a_str   = msg["A"][0]["I"]["a"].as_str().unwrap_or("");

                // Determine if this message is "after crash" in this batch
                let is_after_crash_in_batch = crashed_in_batch
                    || crash_idx.map(|ci| idx > ci).unwrap_or(false);

                match inner_m {
                    // ─── Cashout event
                    "c" => {
                        if let Some(ev) = parse_cashout(a_str, &sub_ctr) {
                            if state.game_state == GameState::Crashed
                                || is_after_crash_in_batch
                            {
                                print_cashout(&ev, true);
                                state.leaked_cashouts.push(ev);
                            } else {
                                print_cashout(&ev, false);
                                state.normal_cashouts.push(ev);
                            }
                        } else {
                            println!(
                                "  {} Failed to parse cashout: {}",
                                "[WARN]".yellow(), a_str
                            );
                        }
                    }

                    // ─── Bet event
                    "b" => {
                        if let Some(ev) = parse_bet(a_str, &sub_ctr) {
                            if state.game_state == GameState::Flying
                                || state.game_state == GameState::Crashed
                            {
                                print_bet(&ev, true);
                                state.leaked_bets.push(ev);
                            } else {
                                print_bet(&ev, false);
                                state.normal_bets.push(ev);
                            }
                        } else {
                            println!(
                                "  {} Failed to parse bet: {}",
                                "[WARN]".yellow(), a_str
                            );
                        }
                    }

                    other => {
                        println!(
                            "  {} Unknown 'g' sub-type '{}' at counter {}",
                            "[UNK]".bright_black(), other, sub_ctr
                        );
                    }
                }
            }

            other => {
                if !other.is_empty() {
                    println!(
                        "  {} Unknown message type '{}' at counter {}",
                        "[UNK]".bright_black(), other, sub_ctr
                    );
                }
            }
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    println!("{}", "╔══════════════════════════════════════════════════════════╗".bright_cyan());
    println!("{}", "║            JetX Data Scraper — Rust Edition              ║".bright_cyan().bold());
    println!("{}", "╚══════════════════════════════════════════════════════════╝".bright_cyan());
    println!();

    // Allow overriding URL via first CLI argument
    let url_str = std::env::args()
        .nth(1)
        .unwrap_or_else(|| WS_URL.to_string());

    let url = match Url::parse(&url_str) {
        Ok(u)  => u,
        Err(e) => {
            eprintln!("{} Invalid URL: {}", "[ERROR]".red().bold(), e);
            std::process::exit(1);
        }
    };

    println!("  Connecting to: {}", url.host_str().unwrap_or("?").yellow());
    println!();

    let mut state = AppState::new();

    loop {
        match connect_async(url.clone()).await {
            Err(e) => {
                eprintln!(
                    "  {} Connection failed: {}. Retrying in 5s…",
                    "[ERROR]".red().bold(), e
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
            Ok((ws_stream, _response)) => {
                println!("  {} WebSocket connected", "[OK]".green().bold());

                let (mut write, mut read) = ws_stream.split();

                // SignalR handshake — send empty JSON init
                let _ = write.send(Message::Text("{}".into())).await;

                while let Some(msg_result) = read.next().await {
                    match msg_result {
                        Ok(Message::Text(text)) => {
                            let t = text.trim();
                            if t.is_empty() || t == "{}" { continue; }
                            process_message(t, &mut state);
                        }
                        Ok(Message::Ping(data)) => {
                            let _ = write.send(Message::Pong(data)).await;
                        }
                        Ok(Message::Close(_)) => {
                            println!(
                                "  {} Server closed connection. Reconnecting…",
                                "[WS]".yellow()
                            );
                            break;
                        }
                        Err(e) => {
                            eprintln!("  {} WS error: {}", "[ERROR]".red(), e);
                            break;
                        }
                        _ => {}
                    }
                }

                println!("  {} Disconnected. Reconnecting in 3s…", "[WS]".yellow());
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        }
    }
}
