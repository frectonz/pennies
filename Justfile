default:
  @just --choose

run:
  cargo r -- serve pennies.toml

check-app1:
  curl http://localhost:3030/api/status -H "Host: app1"

check-app2:
  curl http://localhost:3030/api/status -H "Host: app2"
