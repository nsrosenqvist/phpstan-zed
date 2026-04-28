<?php

declare(strict_types=1);

namespace Fixture;

/**
 * Deliberately buggy class consumed by the integration test. The exact
 * messages and identifiers depend on the PHPStan version, so the test only
 * asserts that *some* diagnostic is produced for this file.
 */
final class Bug
{
    public function undefinedVariable(): int
    {
        // PHPStan: variable.undefined — `$missing` is never defined.
        return $missing + 1;
    }

    public function wrongReturnType(): string
    {
        // PHPStan: return.type — int is not assignable to string.
        return 42;
    }
}
