<?php

declare(strict_types=1);

namespace Fixture;

/**
 * Clean file with no PHPStan errors at level 5.
 */
final class Clean
{
    public function add(int $a, int $b): int
    {
        return $a + $b;
    }
}
