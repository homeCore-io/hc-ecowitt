# hc-ecowitt

[![CI](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore.io/lf-workflow-dash/)

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

## Battery fields

Ecowitt sensors do **not** report a uniform "battery percentage" — the
semantics depend on the sensor family. hc-ecowitt classifies each
incoming value and emits three fields per device:

| field            | type    | meaning                                   |
| ---------------- | ------- | ----------------------------------------- |
| `battery`        | f64     | raw value from the sensor (unchanged)     |
| `battery_low`    | bool    | derived per-kind — true when sensor is low |
| `battery_kind`   | string  | `"binary"`, `"voltage"`, or `"level"`      |

`battery_low` is what rules should trigger off; `battery + battery_kind`
is what dashboards should display.

Taxonomy (per Ecowitt's family-by-family spec, cross-checked against
[`aioecowitt`](https://github.com/home-assistant-libs/aioecowitt)):

| kind                   | raw range | low when     | sensors                                                |
| ---------------------- | --------- | ------------ | ------------------------------------------------------ |
| **binary**             | 0 or 1    | `>= 1`       | WH25, WH26, WH65, WH31 (`batt1..8`), PM2.5 ch 5–8      |
| **voltage** (AA-class) | volts     | `< 1.2`      | WH40, WH68, WH51 soil, WN34 soil temp, WN35 leaf, LDS  |
| **voltage** (supercap) | volts     | `< 2.4`      | WH80, WH85, WH90, WS90                                 |
| **level** (0..=5)      | 0 to 5    | `<= 1`       | WH57 lightning, PM2.5 ch 1–4, leak detectors           |
| **level** (0..=6)      | 0 to 6    | `<= 1`       | WH45 / CO2 5-in-1                                      |

The cloud-API path (`get_livedata_info`) doesn't always tag the sensor
model in its outdoor / rain blocks — those keep the raw `battery`
value but omit `battery_low` and `battery_kind` rather than guess.

## Setup

1. Copy `config/config.toml.example` to `config/config.toml`
2. Configure the Ecowitt gateway's "Customized" upload: Protocol=Ecowitt, Server=this machine's IP, Path=/data/report/, Port=8888
3. Add a `[[plugins]]` entry in `homecore.toml`
