//! Remaking of the `file_io` example using the `Adaptor` type.

use async_filelike::{Adaptor, OpenOptions};
use futures_lite::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    async_io::block_on(async {
        // Create a new file.
        let mut options = OpenOptions::new();
        options.write = true;
        options.create = true;
        let file = options.open("foo.txt").await?;

        // Wrap it in an `Adaptor`.
        let mut file = Adaptor::new(file)?;

        // Write some data to the file
        file.write_all(b"Hello, world!").await?;

        // Close and reopen the file.
        drop(file);
        let mut options = OpenOptions::new();
        options.read = true;
        let file = options.open("foo.txt").await?;

        // Wrap it in an `Adaptor`.
        let mut file = Adaptor::new(file)?;

        // Read the data back.
        let mut buf = [0u8; 13];
        file.read_exact(&mut buf).await?;

        // Print the data.
        println!("{}", String::from_utf8_lossy(&buf[..]));

        // Close the file and delete it.
        drop(file);
        async_filelike::unlink("foo.txt", false).await?;

        Ok(())
    })
}
