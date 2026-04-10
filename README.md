# hc-ecowitt

Bridges Ecowitt weather station sensors into HomeCore. Devices are dynamically discovered from incoming data — no manual sensor configuration needed.

## Data ingestion

- **HTTP POST** (primary) — configure the Ecowitt gateway to POST to this plugin
- **HTTP GET polling** (optional) — plugin polls the gateway's `/get_livedata_info` endpoint

## Supported sensors

All sensors discovered from gateway data are auto-registered. Common types include:

- Temperature (indoor/outdoor/soil/water)
- Humidity
- Barometric pressure
- Wind speed and direction
- Rainfall
- UV index
- Solar radiation
- CO2 / PM2.5

## Setup

1. Copy `config/config.toml.example` to `config/config.toml`
2. Configure the Ecowitt gateway's "Customized" upload: Protocol=Ecowitt, Server=this machine's IP, Path=/data/report/, Port=8888
3. Add a `[[plugins]]` entry in `homecore.toml`
