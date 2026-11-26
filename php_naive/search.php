<?php
// INTENTIONALLY VULNERABLE. Demo only. This is the "quick" search many PHP sites ship.
// Usage: php search.php <database.sqlite> <request.json>
$req  = json_decode(file_get_contents($argv[2]), true);
$max  = $req['max_price_minor'];
$term = $req['search_text'];
$db   = new SQLite3($argv[1], SQLITE3_OPEN_READONLY);
$sql  = "SELECT * FROM courses WHERE published = 1 AND price_minor <= $max AND title LIKE '%$term%'";
$res  = @$db->query($sql);
$rows = [];
while ($res && ($r = $res->fetchArray(SQLITE3_ASSOC))) { $rows[] = $r; }
echo json_encode(['count' => count($rows), 'courses' => $rows], JSON_UNESCAPED_UNICODE);
