# Building a Telegram Bot with Rust and Teloxide

This guide outlines how to build a Telegram bot using the [Teloxide](https://github.com/teloxide/teloxide) library in Rust. Teloxide is a full-featured framework for building Telegram bots, offering both a simple functional API and a more advanced dispatching system.

## Prerequisites

1.  **Rust and Cargo**: Ensure you have Rust installed. If not, install it from [rustup.rs](https://rustup.rs/).
2.  **Telegram Account**: You need a Telegram account to interact with BotFather.
3.  **Bot Token**:
    *   Open Telegram and search for **BotFather** (@BotFather).
    *   Send the command `/newbot`.
    *   Follow the prompts to name your bot and give it a username.
    *   Copy the **HTTP API Token** provided by BotFather.

## 1. Project Setup

Create a new Rust project:

```bash
cargo new my_telegram_bot
cd my_telegram_bot
```

Add the necessary dependencies to your `Cargo.toml`. We'll use `teloxide` for the bot logic, `tokio` for the asynchronous runtime, and `log`/`pretty_env_logger` for logging.

```toml
[package]
name = "my_telegram_bot"
version = "0.1.0"
edition = "2021"

[dependencies]
teloxide = { version = "0.13", features = ["macros"] }
tokio = { version = "1", features = ["full"] }
log = "0.4"
pretty_env_logger = "0.4"
```

*Note: Check [crates.io](https://crates.io/crates/teloxide) for the latest version of `teloxide`.*

## 2. Basic Echo Bot

Let's start with a simple bot that replies to every message with the same text.

Open `src/main.rs` and replace the contents with:

```rust
use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("Starting throw dice bot...");

    let bot = Bot::from_env();

    teloxide::repl(bot, |bot: Bot, msg: Message| async move {
        bot.send_dice(msg.chat.id).await?;
        Ok(())
    })
    .await;
}
```

This uses `teloxide::repl`, a simplified update loop for basic bots.

## 3. Handling Commands

For more complex interactions, you'll want to handle commands (like `/start`, `/help`). Teloxide provides a `BotCommands` derive macro to simplify this.

Update `src/main.rs`:

```rust
use teloxide::{prelude::*, utils::command::BotCommands};

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("Starting command bot...");

    let bot = Bot::from_env();

    Command::repl(bot, answer).await;
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "These commands are supported:")]
enum Command {
    #[command(description = "display this text.")]
    Help,
    #[command(description = "handle a username.")]
    Username(String),
    #[command(description = "handle a username and an age.", parse_with = "split")]
    UsernameAndAge { username: String, age: u8 },
}

async fn answer(bot: Bot, msg: Message, cmd: Command) -> ResponseResult<()> {
    match cmd {
        Command::Help => bot.send_message(msg.chat.id, Command::descriptions().to_string()).await?,
        Command::Username(username) => {
            bot.send_message(msg.chat.id, format!("Your username is @{username}.")).await?
        }
        Command::UsernameAndAge { username, age } => {
            bot.send_message(msg.chat.id, format!("Your username is @{username} and age is {age}."))
                .await?
        }
    };

    Ok(())
}
```

### Explanation
- **`#[derive(BotCommands)]`**: Automatically generates parsing logic for your enum variants.
- **`Command::repl`**: A specialized REPL for handling commands.
- **`answer` function**: Your handler that takes the parsed `Command` and executes logic.

## 4. Running the Bot

To run your bot, you need to provide the `TELOXIDE_TOKEN` environment variable.

**On Linux/macOS:**
```bash
export TELOXIDE_TOKEN=your_token_here
cargo run
```

**On Windows (PowerShell):**
```powershell
$env:TELOXIDE_TOKEN="your_token_here"
cargo run
```

**Using a `.env` file (Optional):**
1. Add `dotenv = "0.15"` to your `Cargo.toml`.
2. Create a `.env` file in the project root: `TELOXIDE_TOKEN=your_token_here`.
3. Call `dotenv::dotenv().ok();` at the start of your `main` function.

## 5. Interactive Keyboards (Inline Buttons)

Text commands are great, but buttons make bots more user-friendly. Here's how to send a message with an **Inline Keyboard** (buttons that appear underneath the message).

### Sending Buttons

You can attach a keyboard to any message using `.reply_markup()`:

```rust
use teloxide::{prelude::*, types::{InlineKeyboardButton, InlineKeyboardMarkup}};

async fn send_keyboard(bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
    // Create a keyboard with two buttons in a single row
    let keyboard = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("Yes, I love it!", "love_rust"),
        InlineKeyboardButton::callback("No, not yet.", "hate_rust"),
    ]]);

    bot.send_message(chat_id, "Do you like Rust?")
        .reply_markup(keyboard)
        .await?;

    Ok(())
}
```

### Handling Button Clicks (Callbacks)

When a user clicks a button, Telegram sends a `CallbackQuery`. To handle both **commands** and **callbacks**, you need to upgrade from the simple `repl` to the **Dispatcher** system.

Here is a full example structure handling both:

```rust
use teloxide::{prelude::*, utils::command::BotCommands};
use std::error::Error;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum Command {
    #[command(description = "Start the bot")]
    Start,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();
    log::info!("Starting advanced bot...");

    let bot = Bot::from_env();

    // Define the handler for commands
    let command_handler = Update::filter_message()
        .filter_command::<Command>()
        .endpoint(answer_command);

    // Define the handler for button clicks (CallbackQuery)
    let callback_handler = Update::filter_callback_query()
        .endpoint(answer_callback);

    // Build the dispatcher
    let handler = dptree::entry()
        .branch(command_handler)
        .branch(callback_handler);

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn answer_command(bot: Bot, msg: Message, cmd: Command) -> ResponseResult<()> {
    match cmd {
        Command::Start => {
            let keyboard = teloxide::types::InlineKeyboardMarkup::new(vec![vec![
                teloxide::types::InlineKeyboardButton::callback("Click me!", "btn_click"),
            ]]);
            bot.send_message(msg.chat.id, "Welcome! Try the button:").reply_markup(keyboard).await?;
        }
    }
    Ok(())
}

async fn answer_callback(bot: Bot, q: CallbackQuery) -> ResponseResult<()> {
    if let Some(data) = q.data {
        // Acknowledge the callback (stop the loading animation)
        bot.answer_callback_query(q.id).await?;
        
        if let Some(Message { chat, .. }) = q.message {
            bot.send_message(chat.id, format!("You clicked: {}", data)).await?;
        }
    }
    Ok(())
}
```

## 6. Next Steps

- **State Management**: Use `dptree` (dispatcher tree) for complex stateful conversations (e.g., a registration wizard).
- **Database Integration**: Connect `sqlx` or `diesel` to store user data.
- **Webhooks**: For production, consider switching from polling (`repl`) to webhooks for better performance.

Refer to the [official Teloxide documentation](https://docs.rs/teloxide/latest/teloxide/) for advanced topics.
