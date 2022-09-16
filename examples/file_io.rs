//! Cross-platform asynchronous file I/O example.

use async_filelike::Handle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    async_io::block_on(async {
        // TODO: handle file opening in-crate
        let file = blocking::unblock(|| std::fs::File::create("foo.txt")).await?;
        let mut file = Handle::new(file)?;

        // Write some data to the file
        println!("Writing data...");
        let (_, res) = file.write_from(b"Hello, world!", 13, 0).await;
        res?;

        // Close and reopen the file.
        drop(file);
        let file = blocking::unblock(|| std::fs::File::open("foo.txt")).await?;
        let mut file = Handle::new(file)?;

        // Read the data back.
        println!("Reading data...");
        let (buf, res) = file.read_into([0u8; 13], 13, 0).await;
        res?;

        // Print the data.
        println!("{}", String::from_utf8_lossy(&buf[..]));

        // Close the file and delete it.
        drop(file);
        blocking::unblock(|| std::fs::remove_file("foo.txt"))
            .await
            .unwrap();

        Ok(())
    })
}
