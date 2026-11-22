# Configure LTO only when CMake enters SDL3's own project().
# This toolchain fragment may be included by other native dependencies: no
# compiler/linker flags are changed for those projects.
set(CMAKE_PROJECT_SDL3_INCLUDE "${CMAKE_CURRENT_LIST_DIR}/sdl3-lto-project.cmake")
