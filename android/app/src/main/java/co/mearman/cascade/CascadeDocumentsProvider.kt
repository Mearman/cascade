package co.mearman.cascade

import android.database.Cursor
import android.database.MatrixCursor
import android.graphics.Point
import android.os.CancellationSignal
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import kotlinx.coroutines.runBlocking
import uniffi.cascade_ffi.CascadeException
import uniffi.cascade_ffi.DirEntry
import java.io.FileNotFoundException

/**
 * Exposes the Cascade VFS to Android's Storage Access Framework.
 *
 * Document IDs are VFS-absolute paths. The root is "/", which the engine presents
 * as the set of mounted backend prefixes (the in-process node mounts a single
 * local backend under the `local` prefix). Listings call [CascadeNode.listDir] and
 * file reads call [CascadeNode.readFile]; nothing here is faked.
 */
class CascadeDocumentsProvider : DocumentsProvider() {

    private companion object {
        const val ROOT_ID = "cascade"
        const val ROOT_DOC_ID = "/"

        val DEFAULT_ROOT_PROJECTION = arrayOf(
            Root.COLUMN_ROOT_ID,
            Root.COLUMN_DOCUMENT_ID,
            Root.COLUMN_TITLE,
            Root.COLUMN_FLAGS,
            Root.COLUMN_ICON,
        )

        val DEFAULT_DOCUMENT_PROJECTION = arrayOf(
            Document.COLUMN_DOCUMENT_ID,
            Document.COLUMN_DISPLAY_NAME,
            Document.COLUMN_MIME_TYPE,
            Document.COLUMN_FLAGS,
            Document.COLUMN_SIZE,
        )
    }

    override fun onCreate(): Boolean = true

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val result = MatrixCursor(projection ?: DEFAULT_ROOT_PROJECTION)
        result.newRow().apply {
            add(Root.COLUMN_ROOT_ID, ROOT_ID)
            add(Root.COLUMN_DOCUMENT_ID, ROOT_DOC_ID)
            add(Root.COLUMN_TITLE, "Cascade")
            add(Root.COLUMN_FLAGS, Root.FLAG_SUPPORTS_IS_CHILD)
            add(Root.COLUMN_ICON, android.R.drawable.ic_menu_save)
        }
        return result
    }

    override fun queryDocument(documentId: String, projection: Array<out String>?): Cursor {
        val result = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        if (documentId == ROOT_DOC_ID) {
            addDirRow(result, ROOT_DOC_ID, "Cascade")
            return result
        }
        // To describe a leaf, list its parent and find the matching entry. This
        // keeps the provider honest: every document it reports came from the
        // engine, not from a fabricated row.
        val parent = parentOf(documentId)
        val name = nameOf(documentId)
        val node = CascadeNodeHolder.blockingGet(requireContext())
        val entry = runBlocking {
            try {
                node.listDir(parent).firstOrNull { it.name == name }
            } catch (e: CascadeException) {
                throw FileNotFoundException("listDir($parent) failed: ${e.message}")
            }
        } ?: throw FileNotFoundException("no such document: $documentId")
        addEntryRow(result, documentId, entry)
        return result
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val result = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        val node = CascadeNodeHolder.blockingGet(requireContext())
        val entries: List<DirEntry> = runBlocking {
            try {
                node.listDir(parentDocumentId)
            } catch (e: CascadeException) {
                throw FileNotFoundException("listDir($parentDocumentId) failed: ${e.message}")
            }
        }
        for (entry in entries) {
            addEntryRow(result, childDocId(parentDocumentId, entry.name), entry)
        }
        return result
    }

    override fun openDocument(
        documentId: String,
        mode: String,
        signal: CancellationSignal?,
    ): ParcelFileDescriptor {
        if (mode != "r") {
            throw UnsupportedOperationException("Cascade documents are read-only via SAF")
        }
        val node = CascadeNodeHolder.blockingGet(requireContext())
        val bytes = runBlocking {
            try {
                node.readFile(documentId)
            } catch (e: CascadeException) {
                throw FileNotFoundException("readFile($documentId) failed: ${e.message}")
            }
        }
        // Stream the content to the caller through a pipe so we never need a temp
        // file on disk. The writer side runs on a background thread.
        val pipe = ParcelFileDescriptor.createReliablePipe()
        val readSide = pipe[0]
        val writeSide = pipe[1]
        Thread({
            ParcelFileDescriptor.AutoCloseOutputStream(writeSide).use { out ->
                try {
                    out.write(bytes)
                } catch (_: Exception) {
                    // The reader closed early; nothing to recover.
                }
            }
        }, "cascade-openDocument").start()
        return readSide
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

    private fun addEntryRow(cursor: MatrixCursor, docId: String, entry: DirEntry) {
        if (entry.isDir) {
            addDirRow(cursor, docId, entry.name)
        } else {
            cursor.newRow().apply {
                add(Document.COLUMN_DOCUMENT_ID, docId)
                add(Document.COLUMN_DISPLAY_NAME, entry.name)
                add(Document.COLUMN_MIME_TYPE, mimeOf(entry.name))
                add(Document.COLUMN_FLAGS, 0)
                add(Document.COLUMN_SIZE, null)
            }
        }
    }

    private fun childDocId(parent: String, name: String): String =
        if (parent == ROOT_DOC_ID) "/$name" else "$parent/$name"

    private fun parentOf(documentId: String): String {
        val trimmed = documentId.trimEnd('/')
        val idx = trimmed.lastIndexOf('/')
        return if (idx <= 0) ROOT_DOC_ID else trimmed.substring(0, idx)
    }

    private fun nameOf(documentId: String): String = documentId.trimEnd('/').substringAfterLast('/')

    private fun mimeOf(name: String): String {
        val ext = name.substringAfterLast('.', "").lowercase()
        val map = android.webkit.MimeTypeMap.getSingleton()
        return map.getMimeTypeFromExtension(ext) ?: "application/octet-stream"
    }

    override fun openDocumentThumbnail(
        documentId: String,
        sizeHint: Point,
        signal: CancellationSignal?,
    ) = null
}
