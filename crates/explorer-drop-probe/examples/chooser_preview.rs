//! Visual preview only: selecting an engine never touches any files.
use vartacopy_shell_windows::transfer::{TransferOperation, TransferOrigin, TransferRequest};

fn main() -> windows::core::Result<()> {
    let request = TransferRequest {
        id: [1; 16],
        operation: if std::env::args().any(|a| a == "--move") {
            TransferOperation::Move
        } else {
            TransferOperation::Copy
        },
        origin: TransferOrigin::Explorer,
        sources: vec![r"D:\Документи\Звіти\Підсумки за жовтень.txt".into()],
        destination: r"E:\Архів\2026\Жовтень".into(),
    };
    println!("{:?}", vartacopy_transfer::choice::show(&request)?);
    Ok(())
}
