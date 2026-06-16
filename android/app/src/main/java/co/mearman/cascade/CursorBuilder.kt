package co.mearman.cascade

import android.database.MatrixCursor
import android.provider.DocumentsContract.Document
import uniffi.cascade_ffi.DirEntry

/**
 * Builds SAF cursors from Cascade directory entries, extracted so the
 * Android-framework cursor logic (column mapping, mime inference, dir-vs-file
 * rows) is unit-testable with Robolectric and a fake `DirEntry` list, with no
 * dependency on the native CascadeNode.
 */
internal object CursorBuilder {

    val DEFAULT_DOCUMENT_PROJECTION = arrayOf(
        Document.COLUMN_DOCUMENT_ID,
        Document.COLUMN_DISPLAY_NAME,
        Document.COLUMN_MIME_TYPE,
        Document.COLUMN_FLAGS,
        Document.COLUMN_SIZE,
    )

    /** A cursor for the children of `parent`, one row per entry. */
    fun childrenCursor(
        parent: String,
        entries: List<DirEntry>,
        projection: Array<out String>?,
    ): MatrixCursor {
        val cursor = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        for (entry in entries) {
            addEntry(cursor, DocIdLogic.childDocId(parent, entry.name), entry)
        }
        return cursor
    }

    /** A single-row cursor describing one entry (a document or the root dir). */
    fun documentCursor(
        docId: String,
        entry: DirEntry,
        projection: Array<out String>?,
    ): MatrixCursor {
        val cursor = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        addEntry(cursor, docId, entry)
        return cursor
    }

    /** A single-row cursor for the root container itself. */
    fun rootDocumentCursor(docId: String, displayName: String, projection: Array<out String>?): MatrixCursor {
        val cursor = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        addDirRow(cursor, docId, displayName)
        return cursor
    }

    private fun addEntry(cursor: MatrixCursor, docId: String, entry: DirEntry) {
        if (entry.isDir) {
            addDirRow(cursor, docId, entry.name)
        } else {
            cursor.newRow().apply {
                add(Document.COLUMN_DOCUMENT_ID, docId)
                add(Document.COLUMN_DISPLAY_NAME, entry.name)
                add(Document.COLUMN_MIME_TYPE, mimeOf(entry.name))
                // FLAG_SUPPORTS_WRITE is honoured via openDocument's write-back
                // path, which uploads on close. Create/delete/rename need a
                // custom SAF call surface the provider does not yet expose, so
                // those flags are omitted to avoid advertising unsupported verbs.
                add(Document.COLUMN_FLAGS, Document.FLAG_SUPPORTS_WRITE)
                add(Document.COLUMN_SIZE, null)
            }
        }
    }

    private fun addDirRow(cursor: MatrixCursor, docId: String, displayName: String) {
        cursor.newRow().apply {
            add(Document.COLUMN_DOCUMENT_ID, docId)
            add(Document.COLUMN_DISPLAY_NAME, displayName)
            add(Document.COLUMN_MIME_TYPE, Document.MIME_TYPE_DIR)
            add(Document.COLUMN_FLAGS, 0)
            add(Document.COLUMN_SIZE, null)
        }
    }

    /** Infer a MIME type from a filename's extension, falling back to a stream. */
    fun mimeOf(name: String): String {
        val ext = name.substringAfterLast('.', "").lowercase()
        if (ext.isEmpty()) return "application/octet-stream"
        val map = android.webkit.MimeTypeMap.getSingleton()
        return map.getMimeTypeFromExtension(ext) ?: "application/octet-stream"
    }
}
