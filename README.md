# Haruki Event Tracker

**Haruki Event Tracker** is a companion project for [HarukiBot](https://github.com/Team-Haruki), designed to track and record in-game ranking data and provide query APIs for clients.

## Requirements
+ `MySQL`, `SQLite`, `PostgreSQL` (Depending on your database choice)
+ `Redis` (For caching borderlines data)

## How to Use
1. Go to release page to download `HarukiEventTracker`
2. Rename `haruki-tracker-configs.example.yaml` to `haruki-tracker-configs.yaml` and then edit it. For more details, see the `haruki-tracker-configs.example.yaml` comments.
3. Make a new directory or use an exists directory
4. Put `HarukiEventTracker` and `haruki-tracker-configs.yaml` in the same directory
5. Open Terminal, and `cd` to the directory
6. Run `HarukiEventTracker`

## License

This project is licensed under the MIT License.