CREATE TABLE IF NOT EXISTS ku_tasks (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    title text NOT NULL CHECK (octet_length(title) BETWEEN 1 AND 800)
);
