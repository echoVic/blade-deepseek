use crossterm::{
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, terminal,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, stdout, Write};
use std::time::Duration;

fn main() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    // Push keyboard enhancement AFTER entering alternate screen (per Kitty protocol spec)
    let enhanced = execute!(
        out,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )
    .is_ok();

    // Write to file since alternate screen won't show println
    let mut log = std::fs::File::create("/tmp/key_debug2.log")?;
    writeln!(log, "Keyboard enhancement enabled: {enhanced}")?;
    writeln!(log, "IN ALTERNATE SCREEN - press keys then q to quit")?;

    loop {
        if event::poll(Duration::from_millis(100))? {
            let ev = event::read()?;
            if let Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = ev
            {
                writeln!(log, "code={code:?}  modifiers={modifiers:?}  kind={kind:?}")?;
                if kind == KeyEventKind::Press && code == KeyCode::Char('q') && modifiers.is_empty()
                {
                    break;
                }
            } else {
                writeln!(log, "OTHER EVENT: {ev:?}")?;
            }
        }
    }

    execute!(out, LeaveAlternateScreen)?;
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    terminal::disable_raw_mode()?;
    println!("Done. Check /tmp/key_debug2.log");
    Ok(())
}
