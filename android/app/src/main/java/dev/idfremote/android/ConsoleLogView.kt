package dev.idfremote.android

import android.content.Context
import android.graphics.Typeface
import android.view.MotionEvent
import android.widget.ScrollView
import android.widget.TextView

/** Bounded console with tail-follow, preserving manual scrollback until returning to the bottom. */
internal class ConsoleLogView(context: Context) : ScrollView(context) {
    private val text = TextView(context).apply {
        typeface = Typeface.MONOSPACE
        textSize = 12f
        setTextIsSelectable(true)
    }
    private var following = true
    private var touching = false
    private var updating = false
    private var restoreY: Int? = null

    init {
        addView(text)
        setOnScrollChangeListener { _, _, _, _, _ ->
            if (!updating) following = !canScrollVertically(1)
        }
    }

    override fun dispatchTouchEvent(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> { touching = true; following = false }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                touching = false
                post { following = !canScrollVertically(1) }
            }
        }
        return super.dispatchTouchEvent(event)
    }

    fun append(value: String) {
        val content = text.text.toString() + value
        val removed = (content.length - 32768).coerceAtLeast(0)
        val oldY = scrollY
        val oldLayout = text.layout
        val removedHeight = if (removed > 0 && oldLayout != null) {
            oldLayout.getLineTop(oldLayout.getLineForOffset(removed.coerceAtMost(text.length())))
        } else 0
        updating = true
        restoreY = (oldY - removedHeight).coerceAtLeast(0)
        text.text = content.takeLast(32768)
        requestLayout()
    }

    override fun onLayout(changed: Boolean, left: Int, top: Int, right: Int, bottom: Int) {
        // TextView replacement can reset its selection and ScrollView's position.
        // Restore the reading anchor only after the new text has been measured.
        updating = true
        super.onLayout(changed, left, top, right, bottom)
        if (!touching) {
            if (following) scrollToBottom()
            else restoreY?.let { scrollTo(0, it) }
        }
        restoreY = null
        updating = false
    }

    private fun scrollToBottom() = scrollTo(0, (text.bottom - height + paddingBottom).coerceAtLeast(0))

    fun followLatest() {
        following = true
        post {
            if (following && !touching) {
                updating = true
                scrollToBottom()
                updating = false
            }
        }
    }

    fun clear() {
        text.text = ""
        followLatest()
    }
}
