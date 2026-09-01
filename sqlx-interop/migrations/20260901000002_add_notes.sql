ALTER TABLE users ADD COLUMN note TEXT;
INSERT INTO users (name, email, note) VALUES ('Ada', 'ada@example.com', 'first');
