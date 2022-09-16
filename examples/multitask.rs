//! Run file I/O on multiple tasks.
//!
//! To demonstrate, this opens every file in the directory, reads the contents,
//! and then determines how many characters there are in every file combined.

use async_channel::bounded;
use async_executor::Executor;
use async_filelike::Adaptor;
use async_io::block_on;
use easy_parallel::Parallel;
use futures_lite::{prelude::*, stream};
use std::{fs, io};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Set up a multithreaded executor.
    let (shutdown, signal) = bounded::<()>(1);
    let executor = Executor::new();
    Parallel::new()
        .each(0..4, |_| {
            // Run tasks from the global queue on this thread.
            block_on(executor.run(signal.recv())).ok();
        })
        .finish(|| {
            block_on(async {
                // Get a list of every file in the current directory.
                let read_dir = blocking::unblock(|| fs::read_dir(".")).await?;
                let read_dir = blocking::Unblock::new(read_dir);

                // Open a new task for every file.
                let tasks = read_dir
                    .filter_map(|entry| {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(err) => return Some(Err(err)),
                        };

                        let path = entry.path();
                        if path.is_file() {
                            // Spawn a task that reads from the file.
                            let task = executor.spawn(async move {
                                // Open the file.
                                let file = blocking::unblock(move || fs::File::open(&path)).await?;
                                let mut file = Adaptor::new(file)?;

                                // Read the contents.
                                let mut contents = vec![];
                                file.read_to_end(&mut contents).await?;

                                // Return the number of characters.
                                Ok::<_, io::Error>(contents.len())
                            });

                            Some(io::Result::Ok(task))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .await;

                // Get the sum of characters.
                let total_len = stream::iter(tasks)
                    .then(|task| async move {
                        match task {
                            Ok(task) => task.await,
                            Err(err) =>  {
                                Err(err)
                            },
                        }
                    })
                    .fold(io::Result::Ok(0), |sum, result| Ok(sum? + result?))
                    .await?;

                println!("Total length: {}", total_len);

                // Shutdown the executor and return.
                drop(shutdown);
                Ok(())
            })
        })
        .1
}
