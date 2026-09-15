#ifndef CODIRA_ABI_H_
#define CODIRA_ABI_H_

#include <stdint.h>

/**
 * Defines the current ABI version
 */
#define CODIRA_ABI_VERSION 300

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
    Gc,
    /**
     * A value struct is allocated on the stack and uses value semantics when
     * passed around.
     *
     * NOTE: When a value struct is used in an external API, a wrapper is
     * created that _pins_ the value on the heap. The heap-allocated value
     * needs to be *manually deallocated*!
     */
    Value,
};
#ifndef __cplusplus
typedef uint8_t CodiraStructMemoryKind;
#endif // __cplusplus

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
    Concrete,
    /**
     * Represents a pointer to a type
     */
    Pointer,
    /**
     * Represents an array of a specific type
     */
    Array,
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
 * Represents a function signature.
 */
typedef struct CodiraFunctionSignature {
    /**
     * Argument types
     */
    const union CodiraTypeId *arg_types;
    /**
     * Optional return type
     */
    union CodiraTypeId return_type;
    /**
     * Number of argument types
     */
    uint16_t num_arg_types;
} CodiraFunctionSignature;

/**
 * Represents a function prototype. A function prototype contains the name,
 * type signature, but not an implementation.
 */
typedef struct CodiraFunctionPrototype {
    /**
     * Function name
     */
    const char *name;
    /**
     * The type signature of the function
     */
    struct CodiraFunctionSignature signature;
} CodiraFunctionPrototype;

/**
 * Represents a function definition. A function definition contains the name,
 * type signature, and a pointer to the implementation.
 *
 * `fn_ptr` can be used to call the declared function.
 */
typedef struct CodiraFunctionDefinition {
    /**
     * Function prototype
     */
    struct CodiraFunctionPrototype prototype;
    /**
     * Function pointer
     */
    const void *fn_ptr;
} CodiraFunctionDefinition;

/**
 * Represents a struct declaration.
 */
typedef struct CodiraStructDefinition {
    /**
     * The unique identifier of this struct
     */
    struct CodiraGuid guid;
    /**
     * Struct fields' names
     */
    const char *const *field_names;
    /**
     * Struct fields' information
     */
    const union CodiraTypeId *field_types;
    /**
     * Struct fields' offsets
     */
    const uint16_t *field_offsets;
    /**
     * Number of fields
     */
    uint16_t num_fields;
    /**
     * Struct memory kind
     */
    CodiraStructMemoryKind memory_kind;
} CodiraStructDefinition;

/**
 * Contains data specific to a group of types that illicit the same
 * characteristics.
 */
enum CodiraTypeDefinitionData_Tag
#ifdef __cplusplus
  : uint8_t
#endif // __cplusplus
 {
    /**
     * Struct types (i.e. record, tuple, or unit structs)
     */
    Struct,
};
#ifndef __cplusplus
typedef uint8_t CodiraTypeDefinitionData_Tag;
#endif // __cplusplus

typedef union CodiraTypeDefinitionData {
    CodiraTypeDefinitionData_Tag tag;
    struct {
        CodiraTypeDefinitionData_Tag struct_tag;
        struct CodiraStructDefinition struct_;
    };
} CodiraTypeDefinitionData;

/**
 * Represents the type declaration for a type that is exported by an assembly.
 *
 * When multiple Codira modules reference the same type, only one module
 * exports the type; the module that contains the type definition. All the
 * other Codira modules reference the type through a [`TypeId`].
 *
 * The modules that defines the type exports the data to reduce the filesize of
 * the assemblies and to ensure only one definition exists. When linking all
 * assemblies together the type definitions from all assemblies are loaded and
 * the information is shared to modules that reference the type.
 *
 * TODO: add support for polymorphism, enumerations, type parameters, generic
 * type definitions, and   constructed generic types.
 */
typedef struct CodiraTypeDefinition {
    /**
     * Type name
     */
    const char *name;
    /**
     * The exact size of the type in bits without any padding
     */
    uint32_t size_in_bits;
    /**
     * The alignment of the type
     */
    uint8_t alignment;
    /**
     * Type group
     */
    union CodiraTypeDefinitionData data;
} CodiraTypeDefinition;

/**
 * Represents a module declaration.
 */
typedef struct CodiraModuleInfo {
    /**
     * Module path
     */
    const char *path;
    /**
     * Module functions
     */
    const struct CodiraFunctionDefinition *functions;
    /**
     * Module types
     */
    const struct CodiraTypeDefinition *types;
    /**
     * Number of module functions
     */
    uint32_t num_functions;
    /**
     * Number of module types
     */
    uint32_t num_types;
} CodiraModuleInfo;

/**
 * Represents a function dispatch table. This is used for runtime linking.
 *
 * Function signatures and pointers are stored separately for cache efficiency.
 */
typedef struct CodiraDispatchTable {
    /**
     * Function signatures
     */
    const struct CodiraFunctionPrototype *prototypes;
    /**
     * Function pointers
     */
    const void **fn_ptrs;
    /**
     * Number of functions
     */
    uint32_t num_entries;
} CodiraDispatchTable;

/**
 * Represents a lookup table for type information. This is used for runtime
 * linking.
 *
 * Type IDs and handles are stored separately for cache efficiency.
 */
typedef struct CodiraTypeLut {
    /**
     * Type IDs
     */
    const union CodiraTypeId *type_ids;
    /**
     * Type information handles
     */
    const void **type_handles;
    /**
     * Debug names
     */
    const char *const *type_names;
    /**
     * Number of types
     */
    uint32_t num_entries;
} CodiraTypeLut;

/**
 * Represents an assembly declaration.
 */
typedef struct CodiraAssemblyInfo {
    /**
     * Symbols of the top-level module
     */
    struct CodiraModuleInfo symbols;
    /**
     * Function dispatch table
     */
    struct CodiraDispatchTable dispatch_table;
    /**
     * Type lookup table
     */
    struct CodiraTypeLut type_lut;
    /**
     * Paths to assembly dependencies
     */
    const char *const *dependencies;
    /**
     * Number of dependencies
     */
    uint32_t num_dependencies;
} CodiraAssemblyInfo;

#endif /* CODIRA_ABI_H_ */
