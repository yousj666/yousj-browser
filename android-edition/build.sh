#!/bin/bash
# Yousj 浏览器安卓版构建（沙箱专用）
# 沙箱的回环 TCP 被拦截，Gradle 守护进程握手必失败；故直接以「与构建完全一致的 JVM 参数」
# 启动 Gradle 启动器，让构建在启动器 JVM 内进程内运行（不分叉守护进程）。
cd "$(dirname "$0")"
source ~/workspace/toolchain/android/env.sh
export GRADLE_USER_HOME=$HOME/workspace/.gradle
export YOUSJ_STORE_PASSWORD=$(grep storePassword signing/passwords.txt | cut -d= -f2)
export YOUSJ_KEY_PASSWORD=$(grep keyPassword signing/passwords.txt | cut -d= -f2)
GL=$HOME/workspace/toolchain/android/gradle/lib
exec java \
  --add-opens=java.base/java.util=ALL-UNNAMED \
  --add-opens=java.base/java.lang=ALL-UNNAMED \
  --add-opens=java.base/java.lang.invoke=ALL-UNNAMED \
  --add-opens=java.prefs/java.util.prefs=ALL-UNNAMED \
  --add-opens=java.base/java.nio.charset=ALL-UNNAMED \
  --add-opens=java.base/java.net=ALL-UNNAMED \
  --add-opens=java.base/java.util.concurrent.atomic=ALL-UNNAMED \
  -Xmx1200m -Dfile.encoding=US-ASCII -Duser.country=US -Duser.language=en -Duser.variant= \
  -javaagent:$GL/agents/gradle-instrumentation-agent-8.7.jar \
  -cp $GL/gradle-launcher-8.7.jar \
  org.gradle.launcher.GradleMain :app:assembleRelease --console=plain
