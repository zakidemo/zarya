<?php
// Any PHP file on the server can read any file the server can read.
// Usage: php plugin.php <path-to-secrets>
echo file_get_contents($argv[1]);
