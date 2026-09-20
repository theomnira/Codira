#ifndef CODIRA_RUNTIME_BINDINGS_H_
#define CODIRA_RUNTIME_BINDINGS_H_

#include <stdbool.h>
#include <stdint.h>

/**
 * Types of primitives supported by Codira.
 */
enum CodiraPrimitiveType
#ifdef __cplusplus
  : uint8_t
#endif // __cplusplus
 {
    CODIRA_PRIMITIVE_TYPE_BOOL,
    CODIRA_PRIMITIVE_TYPE_U8,
    CODIRA_PRIMITIVE_TYPE_U16,
    CODIRA_PRIMITIVE_TYPE_U32,
    CODIRA_PRIMITIVE_TYPE_U64,
    CODIRA_PRIMITIVE_TYPE_U128,
    CODIRA_PRIMITIVE_TYPE_I8,
    CODIRA_PRIMITIVE_TYPE_I16,
    CODIRA_PRIMITIVE_TYPE_I32,
    CODIRA_PRIMITIVE_TYPE_I64,
    CODIRA_PRIMITIVE_TYPE_I128,
    CODIRA_PRIMITIVE_TYPE_F32,
    CODIRA_PRIMITIVE_TYPE_F64,
    CODIRA_PRIMITIVE_TYPE_EMPTY,
    CODIRA_PRIMITIVE_TYPE_VOID,
};
#ifndef __cplusplus
typedef uint8_t CodiraPrimitiveType;
#endif // __cplusplus

/**
 * Represents the kind of memory management a struct uses.
 */
enum CodiraStructMemoryKind
#ifdef __cplusplus
  : uint8_t
#endif // __cplusplus
 {
    /**
     * A garbage collected struct is allocated on the heap and uses reference
     * semantics when passed around.
     */
    CODIRA_STRUCT_MEMORY_KIND_GC,
    /**
     * A value struct is allocated on the stack and uses value semantics when
     * passed around.
     *
     * NOTE: When a value struct is used in an external API, a wrapper is
     * created that _pins_ the value on the heap. The heap-allocated value
     * needs to be *manually deallocated*!
     */
    CODIRA_STRUCT_MEMORY_KIND_VALUE,
};
#ifndef __cplusplus
typedef uint8_t CodiraStructMemoryKind;
#endif // __cplusplus

/**
 * A C-style handle to an error message.
 *
 * If the handle contains a non-null pointer, an error occurred.
 */
typedef struct CodiraErrorHandle {
    const char *error_string;
} CodiraErrorHandle;

/**
 * A C-style handle to a runtime.
 */
typedef struct CodiraRuntime {
    void *_0;
} CodiraRuntime;

/**
 * A [`Type`] holds information about a codira type.
 */
typedef struct CodiraType {
    const void *_0;
    const void *_1;
} CodiraType;

/**
 * A `RawGcPtr` is an unsafe version of a `GcPtr`. It represents the raw
 * internal pointer semantics used by the runtime.
 */
typedef void *const *CodiraRawGcPtr;

/**
 * A `GcPtr` is what you interact with outside of the allocator. It is a
 * pointer to a piece of memory that points to the actual data stored in
 * memory.
 *
 * This creates an indirection that must be followed to get to the actual data
 * of the object. Note that the `GcPtr` must therefore be pinned in memory
 * whereas the contained memory pointer may change.
 */
typedef CodiraRawGcPtr CodiraGcPtr;

/**
 * Definition of an external function that is callable from Codira.
 *
 * The ownership of the contained `TypeInfoHandles` is considered to lie with
 * this struct.
 */
typedef struct CodiraExternalFunctionDefinition {
    /**
     * The name of the function
     */
    const char *name;
    /**
     * The number of arguments of the function
     */
    uint32_t num_args;
    /**
     * The types of the arguments
     */
    const struct CodiraType *arg_types;
    /**
     * The type of the return type
     */
    struct CodiraType return_type;
    /**
     * Pointer to the function
     */
    const void *fn_ptr;
} CodiraExternalFunctionDefinition;

/**
 * Options required to construct a [`RuntimeHandle`] through
 * [`codira_runtime_create`]
 *
 * # Safety
 *
 * This struct contains raw pointers as parameters. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
typedef struct CodiraRuntimeOptions {
    /**
     * Function definitions that should be inserted in the runtime before a
     * codira library is loaded. This is useful to initialize `extern`
     * functions used in a codira library.
     *
     * If the [`num_functions`] fields is non-zero this field must contain a
     * pointer to an array of [`abi::FunctionDefinition`]s.
     */
    const struct CodiraExternalFunctionDefinition *functions;
    /**
     * The number of functions in the [`functions`] array.
     */
    uint32_t num_functions;
} CodiraRuntimeOptions;

/**
 * Describes a `Function` accessible from a Codira [`super::runtime::Runtime`].
 *
 * An instance of `Function` shares ownership of the underlying data. To create
 * a copy of the `Function` object call [`codira_function_add_reference`] to
 * make sure the number of references to the data is properly tracked. Calling
 * [`codira_function_release`] signals the runtime that the data is no longer
 * referenced through the specified object. When all references are released
 * the underlying data is deallocated.
 */
typedef struct CodiraFunction {
    const void *_0;
} CodiraFunction;

/**
 * Represents a globally unique identifier (GUID).
 */
typedef struct CodiraGuid {
    uint8_t _0[16];
} CodiraGuid;

/**
 * Represents a pointer to another type.
 */
typedef struct CodiraPointerTypeId {
    /**
     * The type to which this pointer points
     */
    const union CodiraTypeId *pointee;
    /**
     * Whether or not this pointer is mutable or not
     */
    bool mutable_;
} CodiraPointerTypeId;

/**
 * Represents an array of a specific type.
 */
typedef struct CodiraArrayTypeId {
    /**
     * The element type of the array
     */
    const union CodiraTypeId *element;
} CodiraArrayTypeId;

/**
 * Represents a unique identifier for types. The runtime can use this to lookup
 * the corresponding [`TypeInfo`]. A [`TypeId`] is a key for a [`TypeInfo`].
 *
 * A [`TypeId`] only contains enough information to query the runtime for a
 * [`TypeInfo`].
 */
enum CodiraTypeId_Tag
#ifdef __cplusplus
  : uint8_t
#endif // __cplusplus
 {
    /**
     * Represents a concrete type with a specific Guid
     */
    CODIRA_TYPE_ID_CONCRETE,
    /**
     * Represents a pointer to a type
     */
    CODIRA_TYPE_ID_POINTER,
    /**
     * Represents an array of a specific type
     */
    CODIRA_TYPE_ID_ARRAY,
};
#ifndef __cplusplus
typedef uint8_t CodiraTypeId_Tag;
#endif // __cplusplus

typedef union CodiraTypeId {
    CodiraTypeId_Tag tag;
    struct {
        CodiraTypeId_Tag concrete_tag;
        struct CodiraGuid concrete;
    };
    struct {
        CodiraTypeId_Tag pointer_tag;
        struct CodiraPointerTypeId pointer;
    };
    struct {
        CodiraTypeId_Tag array_tag;
        struct CodiraArrayTypeId array;
    };
} CodiraTypeId;

/**
 * An array of [`Type`]s.
 *
 * The `Types` struct owns the `Type`s it references. Ownership of the `Type`
 * can be shared by calling [`codira_type_add_reference`].
 *
 * This is backed by a dynamically allocated array. Ownership is transferred
 * via this struct and its contents must be destroyed with
 * [`codira_types_destroy`].
 */
typedef struct CodiraTypes {
    const struct CodiraType *types;
    uintptr_t count;
} CodiraTypes;

/**
 * Additional information of a pointer [`Type`].
 *
 * Ownership of this type lies with the [`Type`] that created this instance. As
 * long as the original type is not released through [`codira_type_release`]
 * this type stays alive.
 */
typedef struct CodiraPointerInfo {
    const void *_0;
    const void *_1;
} CodiraPointerInfo;

/**
 * Additional information of a struct [`Type`].
 *
 * Ownership of this type lies with the [`Type`] that created this instance. As
 * long as the original type is not released through [`codira_type_release`]
 * this type stays alive.
 */
typedef struct CodiraStructInfo {
    const void *_0;
    const void *_1;
} CodiraStructInfo;

/**
 * Additional information of an array [`Type`].
 *
 * Ownership of this type lies with the [`Type`] that created this instance. As
 * long as the original type is not released through [`codira_type_release`]
 * this type stays alive.
 */
typedef struct CodiraArrayInfo {
    const void *_0;
    const void *_1;
} CodiraArrayInfo;

/**
 * An enum that defines the kind of type.
 */
enum CodiraTypeKind_Tag
#ifdef __cplusplus
  : uint8_t
#endif // __cplusplus
 {
    CODIRA_TYPE_KIND_PRIMITIVE,
    CODIRA_TYPE_KIND_POINTER,
    CODIRA_TYPE_KIND_STRUCT,
    CODIRA_TYPE_KIND_ARRAY,
};
#ifndef __cplusplus
typedef uint8_t CodiraTypeKind_Tag;
#endif // __cplusplus

typedef union CodiraTypeKind {
    CodiraTypeKind_Tag tag;
    struct {
        CodiraTypeKind_Tag primitive_tag;
        struct CodiraGuid primitive;
    };
    struct {
        CodiraTypeKind_Tag pointer_tag;
        struct CodiraPointerInfo pointer;
    };
    struct {
        CodiraTypeKind_Tag struct_tag;
        struct CodiraStructInfo struct_;
    };
    struct {
        CodiraTypeKind_Tag array_tag;
        struct CodiraArrayInfo array;
    };
} CodiraTypeKind;

/**
 * Information of a field of a struct [`Type`].
 *
 * Ownership of this type lies with the [`Type`] that created this instance. As
 * long as the original type is not released through [`codira_type_release`]
 * this type stays alive.
 */
typedef struct CodiraField {
    const void *_0;
    const void *_1;
} CodiraField;

/**
 * An array of [`Field`]s.
 *
 * This is backed by a dynamically allocated array. Ownership is transferred
 * via this struct and its contents must be destroyed with
 * [`codira_fields_destroy`].
 */
typedef struct CodiraFields {
    const struct CodiraField *fields;
    uintptr_t count;
} CodiraFields;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Writes `count` bytes from `buf` to standard error.
 *
 * # Why this is here and not in the standard library
 *
 * `std/sys/terminate.code` needs to print a panic message, and there is no
 * way for Codira to reach standard error on its own. The portable C
 * spelling is `fwrite(buf, 1, count, stderr)`, and `stderr` is a macro:
 * there is no symbol of that name to declare `extern "C"`, so a Codira
 * declaration cannot name it. Going around it with a file descriptor means
 * `write` on POSIX and `_write` on Windows, which is a per-platform
 * declaration in a standard library that otherwise has none.
 *
 * The runtime already owns platform I/O, so the hook lives here and is one
 * symbol on every target.
 *
 * # Safety
 *
 * `buf` must point to at least `count` readable bytes. A null `buf` or a
 * zero `count` writes nothing and returns, rather than being undefined:
 * an empty panic message is not a reason to fault inside the panic handler.
 */
void codira_write_stderr(const uint8_t *buf, uintptr_t count);

/**
 * Terminates the process abnormally, without unwinding or flushing.
 *
 * Separate from `codira_write_stderr` so that a panic prints its message
 * before the process goes away: a single combined hook would have to decide
 * the message format, which belongs to the standard library.
 */
void codira_abort(void);

/**
 * Terminates the process with `code`, running the usual at-exit handling.
 */
void codira_exit(int32_t code);

/**
 * Whether this build has debug assertions enabled.
 *
 * Reported by the runtime rather than by a compiler intrinsic because it is
 * a property of the build the program is running *in*, and the runtime is
 * what that build produced.
 */
bool codira_is_debug(void);

/**
 * Allocates an object in the runtime of the given `ty`. If successful, `obj`
 * is set, otherwise a non-zero error handle is returned.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_gc_alloc(struct CodiraRuntime runtime,
                                         struct CodiraType ty,
                                         CodiraGcPtr *obj);

/**
 * Retrieves the `ty` for the specified `obj` from the runtime. If successful,
 * `ty` is set, otherwise a non-zero error handle is returned.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_gc_ptr_type(struct CodiraRuntime runtime,
                                            CodiraGcPtr obj,
                                            struct CodiraType *ty);

/**
 * Roots the specified `obj`, which keeps it and objects it references alive.
 * Objects marked as root, must call `codira_gc_unroot` before they can be
 * collected. An object can be rooted multiple times, but you must make sure to
 * call `codira_gc_unroot` an equal number of times before the object can be
 * collected. If successful, `obj` has been rooted, otherwise a non-zero error
 * handle is returned.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_gc_root(struct CodiraRuntime runtime, CodiraGcPtr obj);

/**
 * Unroots the specified `obj`, potentially allowing it and objects it
 * references to be collected. An object can be rooted multiple times, so you
 * must make sure to call `codira_gc_unroot` the same number of times as
 * `codira_gc_root` was called before the object can be collected. If
 * successful, `obj` has been unrooted, otherwise a non-zero error handle is
 * returned.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_gc_unroot(struct CodiraRuntime runtime, CodiraGcPtr obj);

/**
 * Collects all memory that is no longer referenced by rooted objects. If
 * successful, `reclaimed` is set, otherwise a non-zero error handle is
 * returned. If `reclaimed` is `true`, memory was reclaimed, otherwise nothing
 * happend. This behavior will likely change in the future.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_gc_collect(struct CodiraRuntime runtime, bool *reclaimed);

/**
 * Constructs a new runtime that loads the library at `library_path` and its
 * dependencies. If successful, the runtime `handle` is set, otherwise a
 * non-zero error handle is returned.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * The runtime must be manually destructed using [`codira_runtime_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_runtime_create(const char *library_path,
                                               struct CodiraRuntimeOptions options,
                                               struct CodiraRuntime *handle);

/**
 * Destructs the runtime corresponding to `handle`.
 */
struct CodiraErrorHandle codira_runtime_destroy(struct CodiraRuntime runtime);

/**
 * Retrieves the [`FunctionDefinition`] for `fn_name` from the `runtime`. If
 * successful, `has_fn_info` and `fn_info` are set, otherwise a non-zero error
 * handle is returned.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_runtime_find_function_definition(struct CodiraRuntime runtime,
                                                                 const char *fn_name,
                                                                 uintptr_t fn_name_len,
                                                                 bool *has_fn_info,
                                                                 struct CodiraFunction *fn_info);

/**
 * Retrieves the type information corresponding to the specified `type_name`
 * from the runtime. If successful, `has_type_info` and `type_info` are set,
 * otherwise a non-zero error handle is returned.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_runtime_get_type_info_by_name(struct CodiraRuntime runtime,
                                                              const char *type_name,
                                                              bool *has_type_info,
                                                              struct CodiraType *type_info);

/**
 * Retrieves the type information corresponding to the specified `type_id` from
 * the runtime. If successful, `has_type_info` and `type_info` are set,
 * otherwise a non-zero error handle is returned.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_runtime_get_type_info_by_id(struct CodiraRuntime runtime,
                                                            const union CodiraTypeId *type_id,
                                                            bool *has_type_info,
                                                            struct CodiraType *type_info);

/**
 * Updates the runtime corresponding to `handle`. If successful, `updated` is
 * set, otherwise a non-zero error handle is returned.
 *
 * If a non-zero error handle is returned, it must be manually destructed using
 * [`codira_error_destroy`].
 *
 * # Safety
 *
 * This function receives raw pointers as parameters. If any of the arguments
 * is a null pointer, an error will be returned. Passing pointers to invalid
 * data, will lead to undefined behavior.
 */
struct CodiraErrorHandle codira_runtime_update(struct CodiraRuntime runtime, bool *updated);

/**
 * Notifies the runtime an additional references exists to the function. This
 * ensures that the data is kept alive even if [`codira_function_release`] is
 * called for the existing references. Only after all references have been
 * released can the underlying data be deallocated.
 *
 * # Safety
 *
 * This function might be unsafe if the underlying data has already been
 * deallocated by a previous call to [`codira_function_release`].
 */
struct CodiraErrorHandle codira_function_add_reference(struct CodiraFunction function);

/**
 * Notifies the runtime that one of the references to the function is no longer
 * in use. The data may not immediately be destroyed. Only after all references
 * have been released can the underlying data be deallocated.
 *
 * # Safety
 *
 * This function might be unsafe if the underlying data has been deallocated by
 * a previous call to [`codira_function_release`].
 */
struct CodiraErrorHandle codira_function_release(struct CodiraFunction function);

/**
 * Retrieves the function's function pointer.
 *
 * # Safety
 *
 * This function might be unsafe if the underlying data has been deallocated by
 * a previous call to [`codira_function_release`].
 */
struct CodiraErrorHandle codira_function_fn_ptr(struct CodiraFunction function, const void **ptr);

/**
 * Retrieves the function's name.
 *
 * If the function is successful, the caller is responsible for calling
 * [`codira_string_destroy`] on the return pointer.
 *
 * # Safety
 *
 * This function might be unsafe if the underlying data has been deallocated by
 * a previous call to [`codira_function_release`].
 */
struct CodiraErrorHandle codira_function_name(struct CodiraFunction function, const char **name);

/**
 * Retrieves the function's argument types.
 *
 * If successful, ownership of the [`Types`] is transferred to the caller. It
 * must be deallocated with a call to [`codira_types_destroy`].
 *
 * # Safety
 *
 *
 * This function might be unsafe if the underlying data has been deallocated by
 * a previous call to [`codira_function_release`].
 */
struct CodiraErrorHandle codira_function_argument_types(struct CodiraFunction function,
                                                        struct CodiraTypes *arg_types);

/**
 * Retrieves the function's return type.
 *
 * Ownership of the [`Type`] is transferred to the called. It must be released
 * with a call to [`codira_type_release`].
 *
 * # Safety
 *
 * This function might be unsafe if the underlying data has been deallocated by
 * a previous call to [`codira_function_release`].
 */
struct CodiraErrorHandle codira_function_return_type(struct CodiraFunction function,
                                                     struct CodiraType *ty);

/**
 * Deallocates a string that was allocated by the runtime.
 *
 * # Safety
 *
 * This function receives a raw pointer as parameter. Only when the argument is
 * not a null pointer, its content will be deallocated. Passing pointers to
 * invalid data or memory allocated by other processes, will lead to undefined
 * behavior.
 */
void codira_string_destroy(const char *string);

/**
 * Destructs the error message corresponding to the specified handle.
 *
 * # Safety
 *
 * Only call this function on an [`ErrorHandle`] once.
 */
void codira_error_destroy(struct CodiraErrorHandle error);

/**
 * Notifies the runtime that the specified type is no longer used. Any use of
 * the type after calling this function results in undefined behavior.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Type` has been
 * deallocated in a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_type_release(struct CodiraType ty);

/**
 * Increments the usage count of the specified type.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Type` has been
 * deallocated in a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_type_add_reference(struct CodiraType ty);

/**
 * Retrieves the type's name.
 *
 * # Safety
 *
 * The caller is responsible for calling `codira_string_destroy` on the return
 * pointer - if it is not null.
 *
 * This function results in undefined behavior if the passed in `Type` has been
 * deallocated in a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_type_name(struct CodiraType ty, const char **name);

/**
 * Compares two different Types. Returns `true` if the two types are equal. If
 * either of the two types is invalid because for instance it contains null
 * pointers this function returns `false`.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Type`s have
 * been deallocated in a previous call to [`codira_type_release`].
 */
bool codira_type_equal(struct CodiraType a, struct CodiraType b);

/**
 * Returns the storage size required for a type. The storage size does not
 * include any padding to align the size. Call [`codira_type_alignment`] to
 * request the alignment of the type.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Type`s have
 * been deallocated in a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_type_size(struct CodiraType ty, uintptr_t *size);

/**
 * Returns the alignment requirements of the type.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Type`s have
 * been deallocated in a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_type_alignment(struct CodiraType ty, uintptr_t *align);

/**
 * Returns a new [`Type`] that is a pointer to the specified type.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Type`s have
 * been deallocated in a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_type_pointer_type(struct CodiraType ty,
                                                  bool mutable_,
                                                  struct CodiraType *pointer_ty);

/**
 * Returns a new [`Type`] that is an array of the specified type.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Type`s have
 * been deallocated in a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_type_array_type(struct CodiraType ty, struct CodiraType *array_ty);

/**
 * Returns information about what kind of type this is.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Type`s have
 * been deallocated in a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_type_kind(struct CodiraType ty, union CodiraTypeKind *kind);

/**
 * Destroys the contents of a [`Types`] struct.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Types` has
 * been deallocated by a previous call to [`codira_types_destroy`].
 */
struct CodiraErrorHandle codira_types_destroy(struct CodiraTypes types);

/**
 * Returns the type of the elements stored in this type. Ownership is
 * transferred if this function returns successfully.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `ArrayInfo` has
 * been deallocated by a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_array_type_element_type(struct CodiraArrayInfo ty,
                                                        struct CodiraType *element_ty);

/**
 * Returns the type that this instance points to. Ownership is transferred if
 * this function returns successfully.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `PointerType`
 * has been deallocated by a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_pointer_type_pointee(struct CodiraPointerInfo ty,
                                                     struct CodiraType *pointee);

/**
 * Returns true if this is a mutable pointer.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `PointerType`
 * has been deallocated by a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_pointer_is_mutable(struct CodiraPointerInfo ty, bool *mutable_);

/**
 * Returns a [`Type`] that represents the specified primitive type.
 */
struct CodiraType codira_type_primitive(CodiraPrimitiveType primitive_type);

/**
 * Returns the globally unique identifier (GUID) of the struct.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `StructType`
 * has been deallocated by a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_struct_type_guid(struct CodiraStructInfo ty,
                                                 struct CodiraGuid *guid);

/**
 * Returns the type of memory management to apply for the struct.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `StructType`
 * has been deallocated by a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_struct_type_memory_kind(struct CodiraStructInfo ty,
                                                        CodiraStructMemoryKind *memory_kind);

/**
 * Retrieves the field with the given name.
 *
 * The name can be passed as a non nul-terminated string it must be UTF-8
 * encoded.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Fields` has
 * been deallocated by a previous call to [`codira_fields_destroy`].
 */
struct CodiraErrorHandle codira_fields_find_by_name(struct CodiraFields fields,
                                                    const char *name,
                                                    uintptr_t len,
                                                    bool *has_field,
                                                    struct CodiraField *field);

/**
 * Destroys the contents of a [`Fields`] struct.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Fields` has
 * been deallocated by a previous call to [`codira_fields_destroy`].
 */
struct CodiraErrorHandle codira_fields_destroy(struct CodiraFields fields);

/**
 * Retrieves all the fields of the specified struct type.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `StructType`
 * has been deallocated by a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_struct_type_fields(struct CodiraStructInfo ty,
                                                   struct CodiraFields *fields);

/**
 * Returns the name of the field in the parent struct. Ownership of the name is
 * transferred and must be destroyed with [`codira_string_destroy`]. If this
 * function fails a nullptr is returned.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Field` has
 * been deallocated by a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_field_name(struct CodiraField field, const char **name);

/**
 * Returns the type of the field. Ownership of the returned [`Type`] is
 * transferred and must be released with a call to [`codira_type_release`].
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Field` has
 * been deallocated by a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_field_type(struct CodiraField field, struct CodiraType *ty);

/**
 * Returns the offset of the field in bytes from the start of the parent
 * struct.
 *
 * # Safety
 *
 * This function results in undefined behavior if the passed in `Field` has
 * been deallocated by a previous call to [`codira_type_release`].
 */
struct CodiraErrorHandle codira_field_offset(struct CodiraField field, uintptr_t *offset);

#ifdef __cplusplus
} // extern "C"
#endif // __cplusplus

#endif /* CODIRA_RUNTIME_BINDINGS_H_ */
