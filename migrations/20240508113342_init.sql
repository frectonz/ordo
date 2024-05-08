CREATE TABLE IF NOT EXISTS rooms
(
    id         INTEGER PRIMARY KEY NOT NULL,
    vid        TEXT                NOT NULL UNIQUE,
    admin_code TEXT                NOT NULL,
    name       TEXT                NOT NULL,
    options    TEXT                NOT NULL DEFAULT 0,
    created_at TIMESTAMP           NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_rooms_vid ON rooms (vid);
