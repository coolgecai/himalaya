/// Interactive model selection wizard.
///
/// Shown at REPL startup when no model is explicitly configured.
/// Lets the user choose between cloud providers and local Ollama models.
use std::io::{self, IsTerminal, Read, Write};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    style::{self, Stylize},
    terminal::{self, ClearType},
};

/// Result of the model selection wizard.
pub struct ModelSelection {
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

/// Run the interactive model selection wizard.
/// Returns `None` if the terminal is not interactive or the user cancels.
pub fn run_wizard() -> Option<ModelSelection> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }

    // Step 1: cloud or local
    let kind = select_kind("请选择模型来源", &["云端模型 (Cloud)", "本地模型 (Ollama)"])?;

    match kind {
        0 => configure_cloud(),
        _ => configure_local(),
    }
}

// ---------------------------------------------------------------------------
// Step 2a: cloud
// ---------------------------------------------------------------------------

fn configure_cloud() -> Option<ModelSelection> {
    let providers = [
        (
            "Anthropic",
            "https://api.anthropic.com",
            "Himalaya-opus-4-6",
        ),
        ("OpenAI", "https://api.openai.com/v1", "gpt-4o"),
        ("自定义 (Custom)", "", ""),
    ];

    let labels: Vec<&str> = providers.iter().map(|(n, _, _)| *n).collect();
    let idx = select_kind("选择云端服务商", &labels)?;
    let (_, default_url, default_model) = providers[idx];

    let base_url = if default_url.is_empty() {
        let v = prompt_line("  API Base URL: ")?;
        v.trim().to_string()
    } else {
        let v = prompt_line(&format!("  API Base URL [{default_url}]: "))?;
        if v.trim().is_empty() {
            default_url.to_string()
        } else {
            v.trim().to_string()
        }
    };

    let api_key = prompt_line("  API Key: ")?;
    if api_key.trim().is_empty() {
        eprintln!("  API Key 不能为空");
        return None;
    }

    let model = {
        let hint = if default_model.is_empty() {
            String::new()
        } else {
            format!(" [{default_model}]")
        };
        let v = prompt_line(&format!("  模型名称{hint}: "))?;
        if v.trim().is_empty() && !default_model.is_empty() {
            default_model.to_string()
        } else {
            v.trim().to_string()
        }
    };

    if model.is_empty() {
        eprintln!("  模型名称不能为空");
        return None;
    }

    Some(ModelSelection {
        model,
        base_url: Some(base_url),
        api_key: Some(api_key.trim().to_string()),
    })
}

// ---------------------------------------------------------------------------
// Step 2b: local (Ollama)
// ---------------------------------------------------------------------------

fn configure_local() -> Option<ModelSelection> {
    let base_url = "http://127.0.0.1:11434".to_string();

    print!("  正在获取本地模型列表...");
    io::stdout().flush().ok();

    // Try Ollama HTTP API first, fall back to `ollama list` CLI
    let models = fetch_ollama_models(&base_url)
        .ok()
        .filter(|m| !m.is_empty())
        .or_else(|| fetch_ollama_models_via_cli().ok().filter(|m| !m.is_empty()));

    // Clear the status line
    print!("\r{}\r", " ".repeat(40));
    io::stdout().flush().ok();

    match models {
        None => {
            println!();
            eprintln!(
                "  未找到本地模型。请确认 Ollama 已启动，并已运行 `ollama pull <model>` 下载模型。"
            );
            None
        }
        Some(models) => {
            let labels: Vec<&str> = models.iter().map(String::as_str).collect();
            let idx = select_kind("选择本地模型", &labels)?;
            let model = models[idx].clone();

            Some(ModelSelection {
                model,
                base_url: Some(format!("{}/v1", base_url.trim_end_matches('/'))),
                api_key: Some("ollama".to_string()),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Ollama HTTP (no extra deps — plain TCP)
// ---------------------------------------------------------------------------

fn fetch_ollama_models(base_url: &str) -> Result<Vec<String>, String> {
    let host_port = base_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("127.0.0.1:11434");

    let mut stream =
        std::net::TcpStream::connect(host_port).map_err(|e| format!("连接失败: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();

    let request =
        format!("GET /api/tags HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("发送请求失败: {e}"))?;

    let mut body = String::new();
    stream
        .read_to_string(&mut body)
        .map_err(|e| format!("读取响应失败: {e}"))?;

    let json_body = body
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| body.split("\n\n").nth(1))
        .unwrap_or(&body);

    parse_ollama_models(json_body)
}

fn parse_ollama_models(json: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut remaining = json;
    while let Some(pos) = remaining.find("\"name\"") {
        remaining = &remaining[pos + 6..];
        let after_colon = remaining
            .find(':')
            .map(|i| remaining[i + 1..].trim_start())
            .unwrap_or("");
        if let Some(stripped) = after_colon.strip_prefix('"') {
            if let Some(end) = stripped.find('"') {
                names.push(stripped[..end].to_string());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    Ok(names
        .into_iter()
        .filter(|n| seen.insert(n.clone()))
        .collect())
}

/// Fallback: run `ollama list` and parse its text output.
fn fetch_ollama_models_via_cli() -> Result<Vec<String>, String> {
    let output = std::process::Command::new("ollama")
        .arg("list")
        .output()
        .map_err(|e| format!("ollama list 失败: {e}"))?;

    if !output.status.success() {
        return Err("ollama list 返回错误".to_string());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    // Output format: "NAME   ID   SIZE   MODIFIED"
    // Skip the header line, then take the first column of each row.
    let models: Vec<String> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect();

    Ok(models)
}

// ---------------------------------------------------------------------------
// Arrow-key selection widget
// ---------------------------------------------------------------------------

/// Show a titled list and return the chosen index (0-based), or None on cancel.
fn select_kind(title: &str, items: &[&str]) -> Option<usize> {
    let mut stdout = io::stdout();
    let count = items.len();
    let mut selected: usize = 0;

    // Print title (outside raw mode so \n works normally)
    println!();
    println!("  {title}  (↑↓ 选择, Enter 确认, Ctrl-C 取消)");
    println!();

    terminal::enable_raw_mode().ok()?;

    // Drain any input buffered before or shortly after entering raw mode.
    // Use a 50 ms window so delayed key-release events from the previous
    // selection (common on Windows) are also consumed before we start reading.
    while event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
        let _ = event::read();
    }

    // Initial render
    render_list(&mut stdout, items, selected);

    let result = loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Up,
                kind: KeyEventKind::Press,
                ..
            })) => {
                if selected > 0 {
                    selected -= 1;
                    rerender_list(&mut stdout, items, selected, count);
                }
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Down,
                kind: KeyEventKind::Press,
                ..
            })) => {
                if selected + 1 < count {
                    selected += 1;
                    rerender_list(&mut stdout, items, selected, count);
                }
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Enter,
                kind: KeyEventKind::Press,
                ..
            })) => break Some(selected),
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                ..
            }))
            | Ok(Event::Key(KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press,
                ..
            })) => break None,
            _ => {}
        }
    };

    terminal::disable_raw_mode().ok();

    // Move cursor past the list so subsequent output starts on a fresh line
    let lines_below = count - selected;
    let _ = execute!(
        stdout,
        cursor::MoveDown(lines_below as u16),
        style::Print("\r\n")
    );

    result
}

/// Render the list for the first time (cursor is at the top of the list area).
fn render_list(stdout: &mut impl Write, items: &[&str], selected: usize) {
    for (i, item) in items.iter().enumerate() {
        if i == selected {
            let _ = execute!(
                stdout,
                style::PrintStyledContent(format!("  \u{276f} {item}").cyan().bold()),
                style::Print("\r\n")
            );
        } else {
            let _ = execute!(stdout, style::Print(format!("    {item}\r\n")));
        }
    }
}

/// Move back to the top of the list and redraw.
fn rerender_list(stdout: &mut impl Write, items: &[&str], selected: usize, count: usize) {
    let _ = execute!(stdout, cursor::MoveUp(count as u16));
    for (i, item) in items.iter().enumerate() {
        let _ = execute!(stdout, terminal::Clear(ClearType::CurrentLine));
        if i == selected {
            let _ = execute!(
                stdout,
                style::PrintStyledContent(format!("  \u{276f} {item}").cyan().bold()),
                style::Print("\r\n")
            );
        } else {
            let _ = execute!(stdout, style::Print(format!("    {item}\r\n")));
        }
    }
}

// ---------------------------------------------------------------------------
// Simple line prompt (normal mode)
// ---------------------------------------------------------------------------

fn prompt_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut buf = String::new();
    match io::stdin().read_line(&mut buf) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            while matches!(buf.chars().last(), Some('\n' | '\r')) {
                buf.pop();
            }
            Some(buf)
        }
    }
}
