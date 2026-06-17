package co.mearman.cascade

import android.database.Cursor
import android.database.MatrixCursor
import android.graphics.Point
import android.os.CancellationSignal
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.util.Log
import kotlinx.coroutines.runBlocking
import uniffi.cascade_ffi.CascadeException
import uniffi.cascade_ffi.CascadeNode
import uniffi.cascade_ffi.DirEntry
import java.io.FileNotFoundException

/**
 * Exposes the Cascade VFS to Android's Storage Access Framework.
 *
 * Document IDs are VFS-absolute paths. The root is "/", which the engine presents
 * as the set of mounted backend prefixes (the in-process node mounts a single
 * local backend under the `local` prefix). Listings call [CascadeNode.listDir] and
 * file reads call [CascadeNode.readFile]; nothing here is faked.
 *
 * Write verbs (create/delete/rename) are wired through the same node's
 * [CascadeNode.createDir]/[CascadeNode.delete]/[CascadeNode.rename]. Their
 * availability is advertised on each cursor row by [CursorBuilder.flagsForDir] /
 * [CursorBuilder.flagsForFile] so the Files app surfaces the actions.
 */
class CascadeDocumentsProvider : DocumentsProvider() {

    private companion object {
        const val TAG = "CascadeProvider"
        const val ROOT_ID = "cascade"
        const val ROOT_DOC_ID = "/"
        const val AUTHORITY = "co.mearman.cascade.documents"

        val DEFAULT_ROOT_PROJECTION = arrayOf(
            Root.COLUMN_ROOT_ID,
            Root.COLUMN_DOCUMENT_ID,
            Root.COLUMN_TITLE,
            Root.COLUMN_FLAGS,
            Root.COLUMN_ICON,
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
        if (documentId == ROOT_DOC_ID) {
            return CursorBuilder.rootDocumentCursor(ROOT_DOC_ID, "Cascade", projection)
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
        return CursorBuilder.documentCursor(documentId, entry, projection)
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val node = CascadeNodeHolder.blockingGet(requireContext())
        val entries: List<DirEntry> = runBlocking {
            try {
                node.listDir(parentDocumentId)
            } catch (e: CascadeException) {
                throw FileNotFoundException("listDir($parentDocumentId) failed: ${e.message}")
            }
        }
        return CursorBuilder.childrenCursor(parentDocumentId, entries, projection)
    }

    override fun openDocument(
        documentId: String,
        mode: String,
        signal: CancellationSignal?,
    ): ParcelFileDescriptor {
        val node = CascadeNodeHolder.blockingGet(requireContext())
        val isWrite = mode.contains('w') || mode.contains('+')
        if (isWrite) {
            // Write-back: hand the caller the write end of a reliable pipe and,
            // on a background thread, drain the read end into upload(). The
            // reliable pipe carries the comm channel that closeWithError()
            // writes to, so an upload failure can be propagated back to the
            // caller rather than vanishing into the thread's uncaught handler.
            val pipe = ParcelFileDescriptor.createReliablePipe()
            val readSide = pipe[0]
            val writeSide = pipe[1]
            Thread({
                ParcelFileDescriptor.AutoCloseInputStream(readSide).use { input ->
                    val bytes = input.readBytes()
                    try {
                        runBlocking { node.upload(documentId, bytes) }
                    } catch (e: CascadeException) {
                        // The caller has already closed its write end by the time
                        // we get here, so it believes the write succeeded. Log
                        // loudly, and push the failure across the reliable-pipe
                        // comm channel so any caller observing checkError()
                        // sees it. closeWithError() throws IOException, which we
                        // swallow here only because the failure has already been
                        // recorded via Log.e and the pipe status.
                        Log.e(TAG, "write-back upload failed for $documentId: ${e.message}", e)
                        runCatching {
                            readSide.closeWithError("upload($documentId) failed: ${e.message}")
                        }
                    }
                }
            }, "cascade-openDocument-write").start()
            return writeSide
        }

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
                } catch (e: Exception) {
                    // The reader closed early; the upload path owns write-back
                    // errors, this is just the read-side stream draining.
                    Log.w(TAG, "read-side stream interrupted for $documentId: ${e.message}")
                }
            }
        }, "cascade-openDocument").start()
        return readSide
    }

    override fun createDocument(
        parentDocumentId: String,
        displayName: String,
        mimeType: String,
    ): String {
        val node = CascadeNodeHolder.blockingGet(requireContext())
        val childDocId = DocIdLogic.childDocId(parentDocumentId, displayName)
        runBlocking {
            try {
                if (Document.MIME_TYPE_DIR == mimeType) {
                    node.createDir(childDocId)
                } else {
                    // SAF lets the user create a new (empty) file; model that as
                    // an upload of zero bytes so the node's backend creates it.
                    node.upload(childDocId, ByteArray(0))
                }
            } catch (e: CascadeException) {
                throw IllegalStateException(
                    "createDocument($childDocId, mime=$mimeType) failed: ${e.message}",
                    e,
                )
            }
        }
        notifyChange(childDocId)
        return childDocId
    }

    override fun deleteDocument(documentId: String) {
        val node = CascadeNodeHolder.blockingGet(requireContext())
        runBlocking {
            try {
                node.delete(documentId)
            } catch (e: CascadeException) {
                throw IllegalStateException("deleteDocument($documentId) failed: ${e.message}", e)
            }
        }
        notifyChange(documentId)
    }

    override fun renameDocument(documentId: String, displayName: String): String {
        val node = CascadeNodeHolder.blockingGet(requireContext())
        val parent = parentOf(documentId)
        val newDocId = DocIdLogic.childDocId(parent, displayName)
        runBlocking {
            try {
                node.rename(documentId, newDocId)
            } catch (e: CascadeException) {
                throw IllegalStateException(
                    "renameDocument($documentId -> $newDocId) failed: ${e.message}",
                    e,
                )
            }
        }
        notifyChange(documentId)
        notifyChange(newDocId)
        return newDocId
    }

    /**
     * Tell the system the subtree rooted at [documentId] changed so the Files
     * app re-queries and shows the new state. Uses the standard
     * `notifyChange` URI for a document so any open cursor is invalidated.
     */
    private fun notifyChange(documentId: String) {
        val uri = DocumentsContract.buildDocumentUri(AUTHORITY, documentId)
        context?.contentResolver?.notifyChange(uri, null)
    }

    private fun parentOf(documentId: String): String = DocIdLogic.parentOf(documentId)

    private fun nameOf(documentId: String): String = DocIdLogic.nameOf(documentId)

    override fun openDocumentThumbnail(
        documentId: String,
        sizeHint: Point,
        signal: CancellationSignal?,
    ) = null
}

/**
 * Document-id path logic, extracted so it has a single source of truth and can
 * be unit-tested on the JVM without an emulator. Document ids are VFS-absolute
 * paths with the root represented as `/`.
 */
internal object DocIdLogic {
    fun childDocId(parent: String, name: String): String =
        if (parent == "/") "/$name" else "$parent/$name"

    fun parentOf(documentId: String): String {
        val trimmed = documentId.trimEnd('/')
        val idx = trimmed.lastIndexOf('/')
        return if (idx <= 0) "/" else trimmed.substring(0, idx)
    }

    fun nameOf(documentId: String): String = documentId.trimEnd('/').substringAfterLast('/')
}
