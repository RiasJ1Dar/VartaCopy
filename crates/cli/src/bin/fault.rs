//! Separate opt-in test executable. Never part of the default application build.
use std::io::{Read, Write};
use std::path::PathBuf;
use vartacopy_core::{
    control::Control,
    journal::Journal,
    recovery::{self, Phase},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(args.next().ok_or("journal argument missing")?);
    let id: i64 = args
        .next()
        .ok_or("job missing")?
        .into_string()
        .map_err(|_| "job is not UTF-8")?
        .parse()?;
    let phase: Phase = serde_json::from_value(serde_json::Value::String(
        args.next()
            .ok_or("phase missing")?
            .into_string()
            .map_err(|_| "phase is not UTF-8")?,
    ))?;
    let mut journal = Journal::open(&path)?;
    let mut reached = false;
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |event| {
        if !reached && event.phase == phase {
            reached = true;
            println!("READY");
            std::io::stdout().flush()?;
            let mut byte = [0u8; 1];
            std::io::stdin().read_exact(&mut byte)?;
            return Err(vartacopy_core::Error::Journal(
                "fault barrier unexpectedly released".into(),
            ));
        }
        Ok(())
    })?;
    if !reached {
        return Err(format!("fault phase not reached: {report:?}").into());
    }
    Ok(())
}
