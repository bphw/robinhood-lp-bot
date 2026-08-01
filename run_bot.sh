#!/bin/bash
cd /home/bambang/robinhood-lp-bot
RUST_LOG=info ./target/release/robinhood_lp_bot > bot.log 2>&1 &
echo $! > bot.pid
sleep 2
if kill -0 $(cat bot.pid) 2>/dev/null; then
    echo "Bot started, PID $(cat bot.pid)"
else
    echo "Failed to start"
fi
