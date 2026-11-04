package android.net

import java.net.URI

class Uri private constructor(private val raw: String) {
    private val parsed by lazy { runCatching { URI(raw) }.getOrNull() }

    /** Partes tal como están escritas (RFC 3986, apéndice B): `URI` rechaza caracteres que las webs sí usan. */
    private val parts by lazy { SPLIT.find(raw) }

    fun getScheme(): String? = parsed?.scheme
    fun getHost(): String? = parsed?.host
    fun getPath(): String? = parsed?.path
    fun getQuery(): String? = parsed?.query
    fun getPathSegments(): List<String> = getPath()?.split('/')?.filter { it.isNotEmpty() } ?: emptyList()
    fun getLastPathSegment(): String? = getPathSegments().lastOrNull()

    fun getQueryParameter(key: String): String? = getQuery()
        ?.split('&')
        ?.map { it.split('=', limit = 2) }
        ?.firstOrNull { it[0] == key }
        ?.getOrNull(1)
        ?.let { java.net.URLDecoder.decode(it, Charsets.UTF_8) }

    fun buildUpon(): Builder = Builder().apply {
        val p = parts ?: return@apply
        p.groups[2]?.let { scheme(it.value) }
        p.groups[4]?.let { encodedAuthority(it.value) }
        encodedPath(p.groups[5]?.value.orEmpty())
        p.groups[7]?.let { encodedQuery(it.value) }
        p.groups[9]?.let { encodedFragment(it.value) }
    }

    override fun toString() = raw
    override fun equals(other: Any?) = other is Uri && other.raw == raw
    override fun hashCode() = raw.hashCode()

    class Builder {
        private var scheme: String? = null
        private var authority: String? = null
        private var path = ""
        private var query: String? = null
        private var fragment: String? = null

        fun scheme(value: String?) = apply { scheme = value }
        fun authority(value: String?) = apply { authority = value?.let { encode(it, ":@[]") } }
        fun encodedAuthority(value: String?) = apply { authority = value }
        fun path(value: String?) = apply { path = encode(value.orEmpty(), "/") }
        fun encodedPath(value: String?) = apply { path = value.orEmpty() }
        fun appendPath(segment: String) = appendEncodedPath(encode(segment))
        fun appendEncodedPath(segment: String) = apply { path = path.trimEnd('/') + "/" + segment.trimStart('/') }
        fun query(value: String?) = apply { query = value?.let { encode(it, "=&") } }
        fun encodedQuery(value: String?) = apply { query = value }
        fun clearQuery() = apply { query = null }
        fun appendQueryParameter(key: String, value: String?) = apply {
            val pair = encode(key) + "=" + encode(value.orEmpty())
            query = query?.let { "$it&$pair" } ?: pair
        }
        fun fragment(value: String?) = apply { fragment = value?.let { encode(it) } }
        fun encodedFragment(value: String?) = apply { fragment = value }

        fun build(): Uri = Uri(toString())

        override fun toString() = buildString {
            scheme?.let { append(it).append(':') }
            authority?.let { append("//").append(it) }
            if (authority != null && path.isNotEmpty() && !path.startsWith("/")) append('/')
            append(path)
            query?.let { append('?').append(it) }
            fragment?.let { append('#').append(it) }
        }
    }

    companion object {
        private val SPLIT = Regex("""^(([^:/?#]+):)?(//([^/?#]*))?([^?#]*)(\?([^#]*))?(#(.*))?$""")
        private const val UNRESERVED = "_-!.~'()*"

        @JvmField
        val EMPTY = Uri("")

        @JvmStatic
        fun parse(uriString: String): Uri = Uri(uriString)

        @JvmStatic
        fun encode(value: String?): String = encode(value.orEmpty(), "")

        @JvmStatic
        fun encode(value: String?, allow: String?): String = buildString {
            for (byte in value.orEmpty().toByteArray(Charsets.UTF_8)) {
                val c = (byte.toInt() and 0xff).toChar()
                if (c.code < 0x80 && (c.isLetterOrDigit() || c in UNRESERVED || c in allow.orEmpty())) {
                    append(c)
                } else {
                    append('%').append("%02X".format(byte.toInt() and 0xff))
                }
            }
        }
    }
}
