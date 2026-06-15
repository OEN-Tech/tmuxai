/// Live demo: spawn claude in tmux, send a message, parse the response.
///
/// Usage: cargo run --example live_session

use std::path::Path;
use tmux_ai_io::TmuxSession;
use tmux_ai_parser::Parser;
use tmux_ai_parser::profile::CompiledProfile;
use tmux_ai_parser::events::Event;

#[tokio::main]
async fn main() -> Result<(), String> {
    let profile_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap()
        .join("profiles/claude-code.toml");
    let profile = CompiledProfile::load(&profile_path)?;
    let mut parser = Parser::new(profile);

    println!("🚀 Spawning claude in tmux session 'ai-parser-demo'...");
    let mut session = TmuxSession::spawn(
        "ai-parser-demo",
        "claude --dangerously-skip-permissions",
        ".",
    ).await?;

    // Initial capture to get banner
    println!("📸 Capturing initial state...");
    let events = session.capture_and_parse(&mut parser).await?;
    print_events(&events);

    // Send a message
    println!("\n📤 Sending: 'say hello world, nothing else'");
    let events = session.send_and_parse("say hello world, nothing else", &mut parser).await?;
    print_events(&events);

    println!("\n🔍 Parser state: {:?}", parser.state());

    // Clean up
    println!("\n🧹 Killing session...");
    session.kill().await?;
    println!("✅ Done!");

    Ok(())
}

fn print_events(events: &[Event]) {
    for event in events {
        match event {
            Event::Ready => println!("  ⏳ Ready (waiting for input)"),
            Event::AssistantText { text, .. } => println!("  💬 Assistant: {text}"),
            Event::Question { text, choices } => {
                println!("  ❓ Question: {text}");
                for c in choices {
                    println!("     {}) {}", c.key, c.text);
                }
            }
            Event::ToolUse { tool, args } => println!("  🔧 Tool: {tool}({args})"),
            Event::ToolResult { content, .. } => {
                let display: String = content.chars().take(80).collect();
                println!("  📋 Result: {display}");
            }
            Event::SkillLoaded { name } => println!("  📚 Skill: {name}"),
            Event::Thinking { label, .. } => println!("  🧠 Thinking: {label}"),
            Event::StatusBar(info) => println!("  📊 {} | ${:.2} | {}%", info.model, info.cost.unwrap_or(0.0), info.context_pct.unwrap_or(0.0)),
            Event::Checklist { tasks } => {
                println!("  📋 Checklist:");
                for t in tasks {
                    let icon = match t.status {
                        tmux_ai_parser::events::TaskStatus::Done => "✔",
                        tmux_ai_parser::events::TaskStatus::Active => "◼",
                        tmux_ai_parser::events::TaskStatus::Pending => "◻",
                    };
                    println!("     {icon} {}", t.text);
                }
            }
            Event::StateChange { from, to } => println!("  🔄 State: {from:?} → {to:?}"),
            Event::Error { message } => println!("  ❌ Error: {message}"),
            Event::UnrecognizedBlock { raw } => {
                let display: String = raw.chars().take(60).collect();
                println!("  ❔ Unrecognized: {display}");
            }
        }
    }
}
