# Haruki Event Tracker

**Haruki Event Tracker** is a companion project for [HarukiBot](https://github.com/Team-Haruki), designed to track and record in-game ranking data and provide query APIs for clients.


## Requirements

Before using this project, make sure you have a working instance of [Haruki-Sekai-API](https://github.com/Team-Haruki/Haruki-Sekai-API).

## How to Use

1. Copy `configs.example.py` to `configs.py` and configure it as needed.
2. Install [uv](https://github.com/astral-sh/uv) to manage and install project dependencies.
3. Run the following command to install dependencies:
   ```bash
   uv sync
   ```
4. (Optional) If you plan to use MySQL via aiomysql, install:
   ```bash
   uv add aiomysql
   ```
5. (Optional) If you plan to use SQLite via aiosqlite, install:
   ```bash
   uv add aiosqlite
   ```
6. (Optional) If you're on **Linux/macOS**, it's recommended to install [uvloop](https://github.com/MagicStack/uvloop) for better performance:
   ```bash
   uv add uvloop
   ```
7. If you need to change the listening address or other server settings, edit the `hypercorn.toml` file. If you have installed uvloop, uncomment the `worker_class` line in `hypercorn.toml` to enable it.
8. Finally, run the server using:
   ```bash
   hypercorn app:app --config hypercorn.toml
   ```

## License

This project is licensed under the MIT License.
