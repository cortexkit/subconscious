#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args_os().skip(1);
    let mut connection = None;
    while let Some(arg) = args.next() {
        if arg == "--subc" {
            connection = args.next().map(std::path::PathBuf::from);
        }
    }
    let connection = connection.ok_or("ck-inspect requires --subc <connection file>")?;
    ck_inspect::serve(&connection).await
}
