# entity

SeaORM entity definitions — one module per business-plane table (orgs,
projects, users, API keys, audit, billing). Generated-then-curated: keep
schema changes in migrations and mirror them here; coordination-plane state
never lives in these tables (it belongs to fiducia-node's Raft log).
