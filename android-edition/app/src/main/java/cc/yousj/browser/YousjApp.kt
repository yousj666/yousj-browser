package cc.yousj.browser

import android.app.Application
import org.mozilla.geckoview.GeckoRuntime
import org.mozilla.geckoview.GeckoRuntimeSettings

/** 全局单例 GeckoRuntime（整个进程只建一次） */
class YousjApp : Application() {
    companion object {
        @Volatile
        private var instance: GeckoRuntime? = null

        fun runtime(app: Application): GeckoRuntime {
            return instance ?: synchronized(this) {
                instance ?: create(app).also { instance = it }
            }
        }

        private fun create(app: Application): GeckoRuntime {
            val settings = GeckoRuntimeSettings.Builder()
                .javaScriptEnabled(true)
                .build()
            return GeckoRuntime.create(app, settings)
        }
    }
}
