# Invoked by CMake after project(SDL3 ...) initializes the C compiler.
# Leave Rust's LTO profile, Objective-C bridges and other native crates alone.
if(CMAKE_C_COMPILER_ID STREQUAL "MSVC")
  # CMake's IPO implementation emits /GL for the SDL Release objects.
  # SDL itself excludes SDL_mslibc.c from /GL (memcpy/memset safety).
  set(CMAKE_INTERPROCEDURAL_OPTIMIZATION_RELEASE ON)
elseif(CMAKE_C_COMPILER_ID MATCHES "^(Clang|AppleClang)$")
  # CMake's default IPO for modern Clang is ThinLTO; force FULL LTO.
  set(CMAKE_C_COMPILE_OPTIONS_IPO "-flto=full")
  if(CMAKE_CXX_COMPILER_LOADED)
    set(CMAKE_CXX_COMPILE_OPTIONS_IPO "-flto=full")
  endif()
  set(CMAKE_INTERPROCEDURAL_OPTIMIZATION_RELEASE ON)
else()
  message(FATAL_ERROR "OverInk SDL3 Release LTO needs MSVC or Clang, got ${CMAKE_C_COMPILER_ID}")
endif()
message(STATUS "OverInk: SDL3 Release native full LTO enabled (${CMAKE_C_COMPILER_ID})")
