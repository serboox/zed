-- Fixture for db_client's MySQL integration tests (crates/db_client/src/mysql.rs).
-- `test_unbounded_select_is_bounded` needs a real table larger than
-- MAX_RESULT_ROWS (500) to prove an unbounded SELECT gets capped instead of
-- pulling the whole table into memory.
CREATE DATABASE IF NOT EXISTS instruments;
USE instruments;

CREATE TABLE company_owners (
    id INT AUTO_INCREMENT PRIMARY KEY,
    instrument_id INT NOT NULL,
    owner_name VARCHAR(255) NOT NULL,
    shares_held BIGINT NOT NULL
);

DELIMITER $$
CREATE PROCEDURE seed_company_owners()
BEGIN
    DECLARE i INT DEFAULT 0;
    WHILE i < 1000 DO
        INSERT INTO company_owners (instrument_id, owner_name, shares_held)
        VALUES (i % 50, CONCAT('Owner ', i), 1000 + i);
        SET i = i + 1;
    END WHILE;
END$$
DELIMITER ;

CALL seed_company_owners();
DROP PROCEDURE seed_company_owners;
