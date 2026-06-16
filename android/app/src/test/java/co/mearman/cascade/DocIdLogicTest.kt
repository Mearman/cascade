package co.mearman.cascade

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * JVM unit tests for the document-id path logic. These run on the host JVM
 * (no emulator) because the logic is pure string manipulation over VFS-absolute
 * paths — the fiddly trailing-slash and root edge cases that are easy to break.
 */
class DocIdLogicTest {

    @Test
    fun childDocId_under_root_gets_leading_slash() {
        assertEquals("/Documents", DocIdLogic.childDocId("/", "Documents"))
    }

    @Test
    fun childDocId_under_nested_parent_joins_with_slash() {
        assertEquals("/local/Documents/report.txt", DocIdLogic.childDocId("/local/Documents", "report.txt"))
    }

    @Test
    fun parentOf_root_is_root() {
        assertEquals("/", DocIdLogic.parentOf("/"))
    }

    @Test
    fun parentOf_top_level_child_is_root() {
        assertEquals("/", DocIdLogic.parentOf("/local"))
    }

    @Test
    fun parentOf_nested_path_strips_last_segment() {
        assertEquals("/local/Documents", DocIdLogic.parentOf("/local/Documents/report.txt"))
    }

    @Test
    fun parentOf_ignores_trailing_slash() {
        assertEquals("/local", DocIdLogic.parentOf("/local/Documents/"))
    }

    @Test
    fun nameOf_root_is_empty() {
        assertEquals("", DocIdLogic.nameOf("/"))
    }

    @Test
    fun nameOf_returns_last_segment() {
        assertEquals("report.txt", DocIdLogic.nameOf("/local/Documents/report.txt"))
    }

    @Test
    fun nameOf_ignores_trailing_slash() {
        assertEquals("Documents", DocIdLogic.nameOf("/local/Documents/"))
    }
}
