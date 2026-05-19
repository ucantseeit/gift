

## Tree, Commit, Ref, SymbolicRef的实现 与 相关指令的实现

### 一、实现Object::Tree, 复现git write-tree指令
已实现

### 二、实现Object::Commit, 复现git commit-tree指令
1. 根据磁盘上`Commit`类型的存储方式，实现`Object::Commit`类型  
`Commit`对象在磁盘上的存储示例：  
b'commit 181\x00tree f8bf01ea46a11b01860ff2efd97b21e9918d2b61\nauthor Juyang Li <juyang.li@berkeley.edu> 1778137397 +0800\ncommitter Juyang Li <juyang.li@berkeley.edu> 1778137397 +0800\n\nfirst commit\n'  
b'commit 234\x00tree b76b0b2a67b5a7d006500747f1a8d1ded44130f5\nparent 75119193936d4f75807f6fc48dc27c08bb751df0\nauthor Juyang Li <ucantseeit1000@gmail.com> 1778144514 +0800\ncommitter Juyang Li <ucantseeit1000@gmail.com> 1778144514 +0800\n\nsecond commit\n'
包含:  
tree: ObjectSha,   
parents: Vec< ObjectSha >,  
author_name, author_email, author_time,  
committer_name, committer_email, committer_time,   
commit_message  

为`Object::Commit`实现`read_commit(类比read_tree，再包含一个上层一点的版本)`和`to_binary`方法  

2. 实现commit-tree指令
- 输入是一个`Object::Commit`, 返回是它的二进制表示的哈希
- 做的事情：
    1. 调用`to_binary`得到二进制表示
    2. 算出哈希
    3. 将它按路径存储到`<git-dir>.objects/`文件夹中

### 三、实现Ref，复现git update-ref指令
1. 新增`struct Ref`  
```rust
pub struct Ref {
    commit_id: ObjectSha
}
```  

2. 实现从磁盘中读取出`Ref`的函数

3. 实现新建`Ref`并保存到磁盘的函数  

### 四、实现SymbolicRef, 复现git symbolic-ref <sref-name> <path>
1. 新增`struct SymbolicRef`
```rust
pub struct SymbolicRef {
    path: PathBuf  // 与git_dir的相对路径, 指向refs目录下的某个文件
}
```

2. 实现从磁盘中读取出`SymbolicRef`的函数

3. 实现将`SymbolicRef`保存到磁盘中的函数

### 五、完整实现commit流程(不包含logs中的变更)
准备工作：  
新增`enum Head`, 对应`.git/HEAD`中的内容
```rust
enum Head {
    /// symbolic：HEAD相当于是SymbolicRef, 指向.git/refs/heads/xxx
    TargetBranch{ branch_ref_path: PathBuf },
    /// detached：HEAD 文件内直接是 40 hex
    TargetCommit(ObjectSha)
}
```
为它实现一系列方法(包括从磁盘中读, 写入磁盘, 读取到目标commit_id)  

实现commit函数，输入为`commit_message: String`, 做以下事情：
1. 调用`index_root_tree::from_index_file`, 构建`IndexRootTree`
2. 调用`index_root_tree::write_tree`, 保存构造的树
3. 根据`.git/HEAD`保存的信息，确定父commit
    - `.git/HEAD`指向`.git/refs/heads`里面的引用(非detached HEAD): 则根据引用里包含的那个`commit_id`找到父commit
    - `.git/HEAD`内直接保存了一个`commit_id`(detached HEAD):  则这个`commit_id`对应的就是父commit
4. 构造`Object::Commit`, 并保存到磁盘上
5. 更新`.git/HEAD`
    -  非detached HEAD: 将`.git/HEAD`所指向的那个引用文件里保存的commit_id更新成新的commit_id
    - detached HEAD: 将`.git/HEAD`文件里保存的commit_id更新成新的commit_id

### 六、实现git branch <branch-name>
1. 根据HEAD, 得到目标commit_id, 结合branch_name, 创建新Ref
2. 将新Ref写入磁盘

### 七、实现git checkout <commit-id>
准备工作: 
1. 为`IndexRootTree`实现`IndexRootTree::from_tree_object`
2. 为`IndexRootTree`实现`IndexRootTree::to_worktree`, 将`index`区对应的文件树放入工作区  

实际函数：
```rust
pub fn checkout(commit_id: ObjectHash, is_detach: bool) {...}
```
1. 通过commit-id找到对应的commit-object, 读取出来
2. 调用`IndexRootTree::from_tree_object`构建index文件的内容，用这个index文件替代原本的index文件
3. 调用`IndexRootTree::to_worktree`, 将`index`区对应的文件树放入工作区
4. 更新HEAD的内容, 注意区分是否detach

### 八、重构结构
1. Object类型不应该有object_name这一属性, 它是Object类型的键而非值的一部分, 现在这种处理方式会造成冗余
2. index的entries属性应该是BTreeMap而非Vec




