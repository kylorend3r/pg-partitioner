-- Sample native range-partitioned table, monthly, NOT created by pg-partitioner.
-- Mirrors tests/fixtures/04_already_partitioned_range_time.sql: a pre-existing,
-- unregistered partition set — the exact scenario `pg-partitioner register` is for
-- (declaring an already-partitioned table as managed, without pg-partitioner having
-- created the partitioning itself).
--
-- Partition key (created_at) is monthly range. Three months already exist
-- (2026-05, 2026-06, 2026-07); nothing beyond that yet, and no default
-- partition — so after registering, `pg-partitioner maintain` / `doctor` have
-- something real to do (premake future months, flag the missing default).

CREATE TABLE IF NOT EXISTS public.orders (
    id          bigint GENERATED ALWAYS AS IDENTITY,
    created_at  timestamptz NOT NULL,
    customer_id bigint NOT NULL,
    status      text NOT NULL,
    total_cents bigint NOT NULL,
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

CREATE TABLE IF NOT EXISTS public.orders_2026_05
    PARTITION OF public.orders
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');

CREATE TABLE IF NOT EXISTS public.orders_2026_06
    PARTITION OF public.orders
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');

CREATE TABLE IF NOT EXISTS public.orders_2026_07
    PARTITION OF public.orders
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');

-- A helpful index for the partition key on each child (pg-partitioner's
-- `doctor` flags partition sets missing this).
CREATE INDEX IF NOT EXISTS orders_2026_05_created_at_idx ON public.orders_2026_05 (created_at);
CREATE INDEX IF NOT EXISTS orders_2026_06_created_at_idx ON public.orders_2026_06 (created_at);
CREATE INDEX IF NOT EXISTS orders_2026_07_created_at_idx ON public.orders_2026_07 (created_at);

-- Sample data spread across the three existing months.
INSERT INTO public.orders (created_at, customer_id, status, total_cents)
SELECT
    '2026-05-01'::timestamptz + (random() * interval '90 days'),
    1 + (random() * 500)::int,
    (ARRAY['pending', 'shipped', 'delivered', 'cancelled'])[1 + (random() * 3)::int],
    (random() * 20000)::bigint
FROM generate_series(1, 2000) AS gs;

COMMENT ON TABLE public.orders IS 'Demo monthly range-partitioned table, pre-existing and unregistered — register it with pg-partitioner.';
