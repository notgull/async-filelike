//! Cross-platform asynchronous file I/O example.

use async_filelike::{Handle, OpenOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    async_io::block_on(async {
        // Create a new file.
        let mut options = OpenOptions::new();
        options.write = true;
        options.create = true;
        let file = options.open("foo.txt").await?;

        // Wrap the file in a handle.
        let mut file = Handle::new(file)?;

        // Write some data to the file
        println!("Writing data...");
        let (_, res) = file.write_from(b"Hello, world!", 13, 0).await;
        res?;

        // Close and reopen the file.
        drop(file);
        let mut options = OpenOptions::new();
        options.read = true;
        let file = options.open("foo.txt").await?;

        // Wrap it in a handle.
        let mut file = Handle::new(file)?;

        // Read the data back.
        println!("Reading data...");
        let (buf, res) = file.read_into([0u8; 13], 13, 0).await;
        res?;

        // Print the data.
        println!("{}", String::from_utf8_lossy(&buf[..]));

        // Close the file and delete it.
        drop(file);
        async_filelike::unlink("foo.txt", false).await?;

        Ok(())
    })
}
