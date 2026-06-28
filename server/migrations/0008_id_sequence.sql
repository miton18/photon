-- Multi-instance-safe id generation. The in-memory AtomicU64 counter is per
-- instance, so two instances would mint colliding ids (alb_5 on both). A shared
-- Postgres sequence makes every minted id unique across the cluster. Starts high
-- to avoid colliding with the small seed ids (alb_1, ph_3, …).
CREATE SEQUENCE IF NOT EXISTS photon_id_seq START 1000000;
